//! AI search (PLAN 6.3, 7): vectors per content for semantic search.
//!
//! - **Where:** a content goes to the cloud provider only if a live file in a "Cloud erlaubt"
//!   folder has it and no copy (file, trash, old version) lies in a "Nur lokal" folder – checked
//!   when planning, right before sending (a [`provider::Cleared`] is needed for that) and again
//!   when storing the result. Everything else is embedded by `embed-local` in the home network.
//! - **Revoked:** as soon as a content lies in a "Nur lokal" folder (a folder's class changes, a
//!   copy is made or moved there), everything the cloud made from it is deleted and made again
//!   locally; the deletion is logged.
//! - **Once per content:** results are stored per content hash and text version; duplicates,
//!   renaming and moving cost nothing.
//! - **Costs:** the cloud runs only after an administrator started it (or for a trial run of a
//!   few contents) and pauses when the month's budget is spent; the lexical search goes on.

pub mod chunk;
pub mod images;
pub mod pipeline;
pub mod provider;
pub mod vectors;

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::config::AiConfig;
use crate::error::ApiResult;
use crate::files::{content, data_class};
use crate::jobs;
use crate::state::AppState;
use provider::Cleared;

/// Job kinds: work for the cloud and for the home network. Keys: `text:<hash>`, `image:<hash>`.
pub const KIND_CLOUD: &str = "ai_cloud";
pub const KIND_LOCAL: &str = "ai_local";

/// Characters per piece of text (~400–500 tokens) and how far pieces overlap.
pub const CHUNK_CHARS: usize = 1600;
pub const CHUNK_OVERLAP: usize = 200;
/// Pieces per content: in the cloud about the first 16k tokens; on the NAS (slow) the first three.
/// The rest of long texts is found by the lexical search.
pub const CLOUD_CHUNKS: usize = 40;
pub const LOCAL_CHUNKS: usize = 3;

/// Lock held while results are stored or revoked: a result checked before storing is never
/// stored after its content was revoked.
const AI_LOCK: i64 = 0x786c_7278_6169_0000;

/// A vector space: vectors of different models cannot be compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Space {
    /// The cloud's text model ("Cloud erlaubt").
    Cloud,
    /// The home network's text model ("Nur lokal").
    Local,
    /// The home network's picture model, CLIP ("Nur lokal").
    Clip,
}

impl Space {
    pub fn as_str(self) -> &'static str {
        match self {
            Space::Cloud => "cloud",
            Space::Local => "local",
            Space::Clip => "clip",
        }
    }

    /// Is the index binary-quantized (see [`vectors`])?
    pub fn quantized(self) -> bool {
        self == Space::Cloud
    }
}

/// Who does the work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Cloud,
    Local,
}

impl Side {
    pub fn kind(self) -> &'static str {
        match self {
            Side::Cloud => KIND_CLOUD,
            Side::Local => KIND_LOCAL,
        }
    }

    fn of(space: Space) -> Self {
        match space {
            Space::Cloud => Side::Cloud,
            Space::Local | Space::Clip => Side::Local,
        }
    }
}

/// The configured providers and the pipeline's state.
pub struct Ai {
    pub cloud: Option<provider::Cloud>,
    pub local: Option<provider::Local>,
    /// Wakes the workers: new jobs, the cloud was started.
    pub work: tokio::sync::Notify,
    /// Wakes the planner: new texts, a folder's class changed.
    pub plan: tokio::sync::Notify,
    /// Asks the planner for a full check (see [`pipeline::sweep`]).
    pub sweep_now: AtomicBool,
    /// The last error per side, for the administration.
    pub(crate) errors: Mutex<HashMap<Side, String>>,
    pub(crate) running: std::sync::OnceLock<pipeline::Running>,
}

impl Ai {
    pub fn new(cfg: &AiConfig) -> Result<Self, String> {
        Ok(Self {
            cloud: cfg.cloud.as_ref().map(provider::Cloud::new).transpose()?,
            local: cfg.local.as_ref().map(provider::Local::new).transpose()?,
            work: Default::default(),
            plan: Default::default(),
            sweep_now: AtomicBool::new(false),
            errors: Default::default(),
            running: Default::default(),
        })
    }

    pub fn configured(&self) -> bool {
        self.cloud.is_some() || self.local.is_some()
    }

    /// Model and dimension of a space, if configured.
    pub fn model(&self, space: Space) -> Option<(&str, u32)> {
        match space {
            Space::Cloud => self
                .cloud
                .as_ref()
                .map(|c| (c.embed.model.as_str(), c.embed.dim)),
            Space::Local => self
                .local
                .as_ref()
                .map(|l| (l.embed.model.as_str(), l.embed.dim)),
            Space::Clip => self
                .local
                .as_ref()
                .and_then(|l| l.clip.as_ref())
                .map(|c| (c.model.as_str(), c.dim)),
        }
    }

    /// The space for the text of a content: the cloud if it may go there and one is configured,
    /// else the home network.
    pub fn text_space(&self, t: &Target) -> Option<Space> {
        if t.cloud && self.cloud.is_some() {
            Some(Space::Cloud)
        } else if self.local.is_some() {
            Some(Space::Local)
        } else {
            None
        }
    }

    /// The space for a picture: described in the cloud if it may go there and a vision model is
    /// configured, else a CLIP vector in the home network.
    pub fn image_space(&self, t: &Target) -> Option<Space> {
        if t.cloud && self.cloud.as_ref().is_some_and(|c| c.vision.is_some()) {
            Some(Space::Cloud)
        } else if self.local.as_ref().is_some_and(|l| l.clip.is_some()) {
            Some(Space::Clip)
        } else {
            None
        }
    }

    /// What `ai_done` notes for a picture done in a space with the current models.
    pub fn image_done_as(&self, space: Space) -> Option<String> {
        match space {
            Space::Cloud => {
                let c = self.cloud.as_ref()?;
                Some(format!("{}|{}", c.vision.as_ref()?.model, c.embed.model))
            }
            Space::Clip => Some(self.local.as_ref()?.clip.as_ref()?.model.clone()),
            Space::Local => None,
        }
    }

    pub fn error(&self, side: Side) -> Option<String> {
        self.errors.lock().expect("mutex").get(&side).cloned()
    }

    pub(crate) fn set_error(&self, side: Side, e: Option<String>) {
        let mut map = self.errors.lock().expect("mutex");
        match e {
            Some(e) => map.insert(side, e),
            None => map.remove(&side),
        };
    }
}

/// Where a content with live files may be processed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    /// It may go to the cloud.
    pub cloud: bool,
    /// One of its files changed within the last week: it comes first.
    pub recent: bool,
}

/// The targets of contents that live files have (others have none: nothing to do).
pub async fn targets(st: &AppState, hashes: &[Vec<u8>]) -> ApiResult<HashMap<Vec<u8>, Target>> {
    if hashes.is_empty() {
        return Ok(HashMap::new());
    }
    let live: Vec<(Vec<u8>, bool)> = sqlx::query_as(
        "SELECT content_hash, max(coalesce(mtime, created_at)) > now() - interval '7 days'
           FROM nodes WHERE content_hash = ANY($1) AND deleted_at IS NULL AND kind = 'file'
          GROUP BY content_hash",
    )
    .bind(hashes)
    .fetch_all(&st.db)
    .await?;
    let ids: Vec<Vec<u8>> = live.iter().map(|(h, _)| h.clone()).collect();
    let forbidden = data_class::forbidden_contents(&st.db, st.cfg.default_data_class, &ids).await?;
    Ok(live
        .into_iter()
        .map(|(h, recent)| {
            let cloud = !forbidden.contains(&h);
            (h, Target { cloud, recent })
        })
        .collect())
}

/// Checks right before sending that a content may go to the cloud.
pub async fn clear(st: &AppState, hash: &[u8]) -> ApiResult<Option<Cleared>> {
    let forbidden =
        data_class::forbidden_contents(&st.db, st.cfg.default_data_class, &[hash.to_vec()]).await?;
    Ok(forbidden.is_empty().then_some(Cleared { _private: () }))
}

/// Queues the text work for contents whose results are missing or out of date in the space they
/// belong to now. Returns how many jobs were added.
pub async fn queue_texts(st: &AppState, hashes: &[Vec<u8>]) -> ApiResult<u64> {
    if hashes.is_empty() || !st.ai.configured() {
        return Ok(0);
    }
    let texts: Vec<(Vec<u8>, i64)> = sqlx::query_as(
        "SELECT hash, seq FROM content_text WHERE hash = ANY($1) AND length(text) > 0",
    )
    .bind(hashes)
    .fetch_all(&st.db)
    .await?;
    if texts.is_empty() {
        return Ok(0);
    }
    let with_text: Vec<Vec<u8>> = texts.iter().map(|(h, _)| h.clone()).collect();
    let targets = targets(st, &with_text).await?;
    let done: HashMap<(Vec<u8>, String), (String, i64)> =
        sqlx::query_as::<_, (Vec<u8>, String, String, i64)>(
            "SELECT content_hash, space, model, version FROM ai_done
          WHERE content_hash = ANY($1) AND task = 'text'",
        )
        .bind(&with_text)
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .map(|(h, s, m, v)| ((h, s), (m, v)))
        .collect();
    let mut by_side: HashMap<Side, Vec<(String, i16)>> = HashMap::new();
    for (hash, seq) in texts {
        let Some(t) = targets.get(&hash) else {
            continue;
        };
        let Some(space) = st.ai.text_space(t) else {
            continue;
        };
        let Some((model, _)) = st.ai.model(space) else {
            continue;
        };
        let current = done
            .get(&(hash.clone(), space.as_str().to_owned()))
            .is_some_and(|(m, v)| m == model && *v == seq);
        if !current {
            by_side.entry(Side::of(space)).or_default().push((
                format!("text:{}", content::hex(&hash)),
                if t.recent { 10 } else { 0 },
            ));
        }
    }
    let mut added = 0;
    for (side, list) in by_side {
        added += jobs::enqueue(&st.db, side.kind(), &list).await?;
    }
    if added > 0 {
        st.ai.work.notify_waiters();
    }
    Ok(added)
}

/// Queues the picture work for contents with live picture files whose results are missing or
/// were made with other models. Returns how many jobs were added.
pub async fn queue_images(st: &AppState, hashes: &[Vec<u8>]) -> ApiResult<u64> {
    let ai = &st.ai;
    let possible = ai.cloud.as_ref().is_some_and(|c| c.vision.is_some())
        || ai.local.as_ref().is_some_and(|l| l.clip.is_some());
    if hashes.is_empty() || !possible {
        return Ok(0);
    }
    let pictures: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT DISTINCT content_hash FROM nodes
          WHERE content_hash = ANY($1) AND deleted_at IS NULL AND kind = 'file'
            AND size BETWEEN $2 AND $3 AND name ~* $4",
    )
    .bind(hashes)
    .bind(images::MIN_BYTES)
    .bind(crate::files::thumbs::MAX_INPUT as i64)
    .bind(images::NAME_PATTERN)
    .fetch_all(&st.db)
    .await?;
    if pictures.is_empty() {
        return Ok(0);
    }
    let targets = targets(st, &pictures).await?;
    let done: HashMap<(Vec<u8>, String), String> = sqlx::query_as::<_, (Vec<u8>, String, String)>(
        "SELECT content_hash, space, model FROM ai_done
              WHERE content_hash = ANY($1) AND task = 'image'",
    )
    .bind(&pictures)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .map(|(h, s, m)| ((h, s), m))
    .collect();
    let mut by_side: HashMap<Side, Vec<(String, i16)>> = HashMap::new();
    for hash in pictures {
        let Some(t) = targets.get(&hash) else {
            continue;
        };
        let Some(space) = ai.image_space(t) else {
            continue;
        };
        let Some(done_as) = ai.image_done_as(space) else {
            continue;
        };
        if done.get(&(hash.clone(), space.as_str().to_owned())) != Some(&done_as) {
            by_side.entry(Side::of(space)).or_default().push((
                format!("image:{}", content::hex(&hash)),
                if t.recent { 10 } else { 0 },
            ));
        }
    }
    let mut added = 0;
    for (side, list) in by_side {
        added += jobs::enqueue(&st.db, side.kind(), &list).await?;
    }
    if added > 0 {
        ai.work.notify_waiters();
    }
    Ok(added)
}

/// A result to store.
pub(crate) struct Outcome<'a> {
    pub hash: &'a [u8],
    pub space: Space,
    /// The model of the vectors.
    pub model: &'a str,
    /// What `ai_done` notes: the model (for pictures in the cloud also the vision model).
    pub done_as: &'a str,
    /// `content_text.seq` of the text embedded; 0 for pictures.
    pub version: i64,
    /// `text` or `image`.
    pub task: &'static str,
    /// `text`, `description` or `image`.
    pub source: &'static str,
    pub pieces: Vec<vectors::Piece>,
    /// A picture description and the model that made it.
    pub vision: Option<(&'a str, &'a provider::Seen)>,
}

/// Notes that pictures' descriptions changed (the search index follows `ai_vision_log`). Within
/// a transaction holding the AI lock, so numbers become visible in order.
async fn log_vision(tx: &mut sqlx::PgConnection, hashes: &[Vec<u8>]) -> ApiResult<()> {
    if !hashes.is_empty() {
        sqlx::query("INSERT INTO ai_vision_log (content_hash) SELECT unnest($1::bytea[])")
            .bind(hashes)
            .execute(&mut *tx)
            .await?;
    }
    Ok(())
}

/// Stores a result. For the cloud, checks once more under the lock that the content may be
/// there; false if not (nothing stored).
pub(crate) async fn store(st: &AppState, o: Outcome<'_>) -> ApiResult<bool> {
    let mut tx = st.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AI_LOCK)
        .execute(&mut *tx)
        .await?;
    if o.space == Space::Cloud && clear(st, o.hash).await?.is_none() {
        return Ok(false);
    }
    vectors::replace(&mut tx, o.hash, o.space, o.model, o.source, &o.pieces).await?;
    sqlx::query(
        "INSERT INTO ai_done (content_hash, space, task, model, version) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (content_hash, space, task) DO UPDATE
            SET model = EXCLUDED.model, version = EXCLUDED.version, at = now()",
    )
    .bind(o.hash)
    .bind(o.space.as_str())
    .bind(o.task)
    .bind(o.done_as)
    .bind(o.version)
    .execute(&mut *tx)
    .await?;
    if let Some((model, seen)) = o.vision {
        sqlx::query(
            "INSERT INTO ai_vision
                (content_hash, model, description, tags, text_in_image, doc_type, date_found)
             VALUES ($1, $2, $3, $4, $5, $6, $7)
             ON CONFLICT (content_hash) DO UPDATE
                SET model = EXCLUDED.model, description = EXCLUDED.description,
                    tags = EXCLUDED.tags, text_in_image = EXCLUDED.text_in_image,
                    doc_type = EXCLUDED.doc_type, date_found = EXCLUDED.date_found, at = now()",
        )
        .bind(o.hash)
        .bind(model)
        .bind(&seen.description)
        .bind(&seen.tags)
        .bind(&seen.text)
        .bind(&seen.doc_type)
        .bind(seen.date)
        .execute(&mut *tx)
        .await?;
        log_vision(&mut tx, &[o.hash.to_vec()]).await?;
    }
    if o.space == Space::Cloud {
        // In the cloud now: the home network's (weaker) results are no longer needed.
        let local = if o.task == "text" { "local" } else { "clip" };
        sqlx::query("DELETE FROM ai_vectors WHERE content_hash = $1 AND space = $2")
            .bind(o.hash)
            .bind(local)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM ai_done WHERE content_hash = $1 AND space = $2 AND task = $3")
            .bind(o.hash)
            .bind(local)
            .bind(o.task)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Deletes everything the cloud made from these contents (vectors, picture descriptions), logs
/// it and queues the work in the home network. Returns how many contents had results.
pub async fn revoke(st: &AppState, hashes: &[Vec<u8>], reason: &str) -> ApiResult<u64> {
    if hashes.is_empty() {
        return Ok(0);
    }
    let mut tx = st.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AI_LOCK)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM ai_vectors WHERE space = 'cloud' AND content_hash = ANY($1)")
        .bind(hashes)
        .execute(&mut *tx)
        .await?;
    let mut gone: HashSet<Vec<u8>> = sqlx::query_scalar(
        "DELETE FROM ai_done WHERE space = 'cloud' AND content_hash = ANY($1) RETURNING content_hash",
    )
    .bind(hashes)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let seen: Vec<Vec<u8>> = sqlx::query_scalar(
        "DELETE FROM ai_vision WHERE content_hash = ANY($1) RETURNING content_hash",
    )
    .bind(hashes)
    .fetch_all(&mut *tx)
    .await?;
    log_vision(&mut tx, &seen).await?;
    gone.extend(seen);
    tx.commit().await?;
    if gone.is_empty() {
        return Ok(0);
    }
    let gone: Vec<Vec<u8>> = gone.into_iter().collect();
    crate::audit::log(
        &st.db,
        None,
        None,
        "ai_revoked",
        None,
        serde_json::json!({ "contents": gone.len(), "reason": reason }),
    )
    .await?;
    tracing::info!(
        contents = gone.len(),
        reason,
        "KI: Cloud-Ergebnisse gelöscht"
    );
    queue_texts(st, &gone).await?;
    queue_images(st, &gone).await?;
    Ok(gone.len() as u64)
}

/// Deletes the results of contents no file, file in the trash or old version has any more (a
/// file restored from the trash keeps its results). Returns how many.
pub async fn forget_gone(st: &AppState) -> ApiResult<u64> {
    let mut tx = st.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AI_LOCK)
        .execute(&mut *tx)
        .await?;
    let exists = "EXISTS (SELECT 1 FROM nodes n WHERE n.content_hash = x.content_hash
                     AND (n.deleted_at IS NULL OR EXISTS (
                           SELECT 1 FROM nodes t WHERE t.id = n.deleted_with AND t.trash_path IS NOT NULL)))
                  OR EXISTS (SELECT 1 FROM versions v WHERE v.content_hash = x.content_hash)";
    let mut gone: HashSet<Vec<u8>> = sqlx::query_scalar::<_, Vec<u8>>(sqlx::AssertSqlSafe(
        format!("DELETE FROM ai_done x WHERE NOT ({exists}) RETURNING content_hash"),
    ))
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let seen: Vec<Vec<u8>> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "DELETE FROM ai_vision x WHERE NOT ({exists}) RETURNING content_hash"
    )))
    .fetch_all(&mut *tx)
    .await?;
    log_vision(&mut tx, &seen).await?;
    gone.extend(seen);
    let gone: Vec<Vec<u8>> = gone.into_iter().collect();
    sqlx::query("DELETE FROM ai_vectors WHERE content_hash = ANY($1)")
        .bind(&gone)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(gone.len() as u64)
}

/// Of these contents, revokes the cloud results of those now lying in a "Nur lokal" folder.
pub async fn revoke_local(st: &AppState, hashes: &[Vec<u8>], reason: &str) -> ApiResult<u64> {
    if hashes.is_empty() {
        return Ok(0);
    }
    // Every cloud result (also a picture description) has its row here.
    let cloudy: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT DISTINCT content_hash FROM ai_done WHERE space = 'cloud' AND content_hash = ANY($1)",
    )
    .bind(hashes)
    .fetch_all(&st.db)
    .await?;
    let local: Vec<Vec<u8>> =
        data_class::local_contents(&st.db, st.cfg.default_data_class, &cloudy)
            .await?
            .into_iter()
            .collect();
    revoke(st, &local, reason).await
}

/// Contents below a folder (files, also deleted ones, and their old versions).
async fn contents_below(st: &AppState, folder: i64) -> ApiResult<Vec<Vec<u8>>> {
    Ok(sqlx::query_scalar(
        "WITH RECURSIVE t AS (
            SELECT id, 0 AS depth FROM nodes WHERE id = $1
            UNION ALL
            SELECT n.id, t.depth + 1 FROM nodes n JOIN t ON n.parent_id = t.id WHERE t.depth < 1000
         )
         SELECT DISTINCT h FROM (
            SELECT n.content_hash AS h FROM t JOIN nodes n ON n.id = t.id
             WHERE n.content_hash IS NOT NULL
            UNION ALL
            SELECT v.content_hash FROM t JOIN versions v ON v.node_id = t.id
         ) x",
    )
    .bind(folder)
    .fetch_all(&st.db)
    .await?)
}

/// A folder's data class changed: revokes the cloud results of what is "Nur lokal" now, and
/// queues what is missing in the space its contents belong to now.
pub async fn class_changed(st: &AppState, folder: i64) -> ApiResult<()> {
    let hashes = contents_below(st, folder).await?;
    for part in hashes.chunks(5000) {
        // Also results of an earlier configuration.
        revoke_local(st, part, "Ordner ist jetzt „Nur lokal“").await?;
        queue_texts(st, part).await?;
        queue_images(st, part).await?;
    }
    Ok(())
}

/// The start of the current month in the household's time zone.
const MONTH: &str = "date_trunc('month', now() AT TIME ZONE $1)::date";

/// Notes a call to the cloud and what it cost.
pub async fn record_usage(
    st: &AppState,
    kind: &str,
    tokens_in: u64,
    tokens_out: u64,
    cost: f64,
) -> ApiResult<()> {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO ai_usage (month, kind, calls, tokens_in, tokens_out, cost)
         VALUES ({MONTH}, $2, 1, $3, $4, $5)
         ON CONFLICT (month, kind) DO UPDATE
            SET calls = ai_usage.calls + 1, tokens_in = ai_usage.tokens_in + EXCLUDED.tokens_in,
                tokens_out = ai_usage.tokens_out + EXCLUDED.tokens_out,
                cost = ai_usage.cost + EXCLUDED.cost"
    )))
    .bind(&st.cfg.timezone)
    .bind(kind)
    .bind(tokens_in as i64)
    .bind(tokens_out as i64)
    .bind(cost)
    .execute(&st.db)
    .await?;
    Ok(())
}

/// Spent this month, in euros.
pub async fn month_spent(st: &AppState) -> ApiResult<f64> {
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT coalesce(sum(cost), 0)::float8 FROM ai_usage WHERE month = {MONTH}"
    )))
    .bind(&st.cfg.timezone)
    .fetch_one(&st.db)
    .await?)
}

/// Why the cloud may not spend `estimate` euros now, if it may not.
pub async fn cloud_paused(st: &AppState, estimate: f64) -> ApiResult<Option<&'static str>> {
    if st.ai.cloud.is_none() {
        return Ok(Some("Kein Cloud-Anbieter eingerichtet"));
    }
    let (on, trial, budget): (bool, Option<i32>, Option<f64>) =
        sqlx::query_as("SELECT cloud_on, trial_left, budget FROM ai_state")
            .fetch_one(&st.db)
            .await?;
    if !on && trial.unwrap_or(0) <= 0 {
        return Ok(Some("Die Cloud-Analyse ist nicht gestartet"));
    }
    let budget = budget.unwrap_or(st.cfg.ai.budget);
    let spent = month_spent(st).await?;
    if spent >= budget || spent + estimate > budget {
        return Ok(Some("Das Monatsbudget ist ausgeschöpft"));
    }
    Ok(None)
}

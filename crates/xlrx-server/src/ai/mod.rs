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

/// Job kinds: work for the cloud and for the home network. Keys: `text:<hash>`.
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
    /// The home network's picture model ("Nur lokal", M4.2).
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
            Space::Clip => None,
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

/// Stores the result of a content in a space. For the cloud, checks once more under the lock
/// that the content may be there; false if not (nothing stored).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn store(
    st: &AppState,
    hash: &[u8],
    space: Space,
    model: &str,
    version: i64,
    task: &str,
    source: &str,
    pieces: &[vectors::Piece],
) -> ApiResult<bool> {
    let mut tx = st.db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(AI_LOCK)
        .execute(&mut *tx)
        .await?;
    if space == Space::Cloud && clear(st, hash).await?.is_none() {
        return Ok(false);
    }
    vectors::replace(&mut tx, hash, space, model, source, pieces).await?;
    sqlx::query(
        "INSERT INTO ai_done (content_hash, space, task, model, version) VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (content_hash, space, task) DO UPDATE
            SET model = EXCLUDED.model, version = EXCLUDED.version, at = now()",
    )
    .bind(hash)
    .bind(space.as_str())
    .bind(task)
    .bind(model)
    .bind(version)
    .execute(&mut *tx)
    .await?;
    if space == Space::Cloud && task == "text" {
        // In the cloud now: the home network's (weaker) vectors are no longer needed.
        sqlx::query("DELETE FROM ai_vectors WHERE content_hash = $1 AND space = 'local'")
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM ai_done WHERE content_hash = $1 AND space = 'local'")
            .bind(hash)
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
    gone.extend(
        sqlx::query_scalar::<_, Vec<u8>>(
            "DELETE FROM ai_vision WHERE content_hash = ANY($1) RETURNING content_hash",
        )
        .bind(hashes)
        .fetch_all(&mut *tx)
        .await?,
    );
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

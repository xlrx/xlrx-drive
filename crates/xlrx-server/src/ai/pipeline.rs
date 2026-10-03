//! The AI pipeline (PLAN 6.6): a planner follows the extracted texts and the journal and queues
//! work; workers for the cloud and for the home network do it.
//!
//! The planner also revokes cloud results as soon as a content lies in a "Nur lokal" folder,
//! and every few hours checks everything once more ([`sweep`]): results of contents that may no
//! longer be in the cloud, of contents no file has any more, and work that is missing.

use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::watch;

use super::images::{self, Read};
use super::provider::Seen;
use super::provider::{CallError, estimate_tokens};
use super::vectors::{self, Piece};
use super::{CHUNK_CHARS, CHUNK_OVERLAP, CLOUD_CHUNKS, LOCAL_CHUNKS, Side, Space};
use super::{Outcome, chunk};
use crate::error::ApiError;
use crate::files::content;
use crate::jobs::{self, Job};
use crate::state::AppState;

/// Texts and journal entries per planner round.
const BATCH: i64 = 1000;
/// Contents per step of the full check.
const SWEEP_BATCH: i64 = 2000;
/// The full check runs this often (and at start).
const SWEEP_EVERY: Duration = Duration::from_secs(6 * 3600);
/// Without notification, the planner and the workers look this often.
const POLL: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);
/// After the provider refused the access: wait before asking again.
const ACCESS_PAUSE: Duration = Duration::from_secs(10 * 60);

/// The running planner and workers.
pub struct Running {
    stop: watch::Sender<bool>,
    tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Running {
    /// Stops planner and workers once their current step is done.
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        for t in self.tasks.lock().await.drain(..) {
            let _ = t.await;
        }
    }
}

/// Starts the planner and the workers (if a provider is configured) and creates the vector
/// indexes of the configured models in the background.
pub fn start(st: &AppState) -> Result<(), String> {
    let ai = &st.ai;
    if !ai.configured() {
        tracing::info!("KI-Suche nicht eingerichtet");
        return Ok(());
    }
    let mut indexes = Vec::new();
    for space in [Space::Cloud, Space::Local] {
        if let Some((model, dim)) = ai.model(space) {
            indexes.push((space, model.to_owned(), dim));
        }
    }
    let db = st.db.clone();
    tokio::spawn(async move {
        for (space, model, dim) in indexes {
            if let Err(e) = vectors::ensure_index(&db, space, &model, dim).await {
                tracing::warn!(error = %e, model, "KI: Vektorindex nicht angelegt");
            }
        }
    });
    tracing::info!(
        cloud = ai.cloud.as_ref().map(|c| c.embed.model.as_str()),
        local = ai.local.as_ref().map(|l| l.embed.model.as_str()),
        "KI-Suche"
    );
    let (stop, stop_rx) = watch::channel(false);
    let mut tasks = vec![tokio::spawn(planner(st.clone(), stop_rx.clone()))];
    if ai.cloud.is_some() {
        for _ in 0..st.cfg.ai.cloud_workers {
            tasks.push(tokio::spawn(worker(
                st.clone(),
                Side::Cloud,
                stop_rx.clone(),
            )));
        }
    }
    if ai.local.is_some() {
        // The NAS has few cores: one piece of work at a time.
        tasks.push(tokio::spawn(worker(
            st.clone(),
            Side::Local,
            stop_rx.clone(),
        )));
    }
    ai.running
        .set(Running {
            stop,
            tasks: tokio::sync::Mutex::new(tasks),
        })
        .map_err(|_| "Die KI-Suche läuft schon".to_string())
}

async fn planner(st: AppState, mut stop: watch::Receiver<bool>) {
    let mut last_sweep: Option<tokio::time::Instant> = None;
    loop {
        if *stop.borrow() {
            return;
        }
        if last_sweep.is_none_or(|t| t.elapsed() > SWEEP_EVERY)
            || st.ai.sweep_now.swap(false, Ordering::SeqCst)
        {
            last_sweep = Some(tokio::time::Instant::now());
            if let Err(e) = sweep(&st).await {
                tracing::warn!(error = ?e, "KI: Prüfung fehlgeschlagen");
            }
        }
        match plan(&st).await {
            Ok(true) => continue,
            Ok(false) => {}
            Err(_) if st.db.is_closed() => return,
            Err(e) => tracing::warn!(error = ?e, "KI: Planen fehlgeschlagen"),
        }
        tokio::select! {
            () = st.ai.plan.notified() => {}
            () = tokio::time::sleep(POLL) => {}
            _ = stop.changed() => return,
        }
    }
}

#[derive(sqlx::FromRow)]
struct Entry {
    seq: i64,
    kind: String,
    op: String,
    reparented: Option<bool>,
    node_id: i64,
    content_hash: Option<Vec<u8>>,
}

/// One planner round: new texts and journal entries since the last round. True if more are
/// waiting.
pub async fn plan(st: &AppState) -> Result<bool, ApiError> {
    let (text_seq, journal_seq): (i64, i64) =
        sqlx::query_as("SELECT text_seq, journal_seq FROM ai_state")
            .fetch_one(&st.db)
            .await?;
    let texts: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, hash FROM content_text WHERE seq > $1 ORDER BY seq LIMIT $2")
            .bind(text_seq)
            .bind(BATCH)
            .fetch_all(&st.db)
            .await?;
    let entries: Vec<Entry> = sqlx::query_as(
        "SELECT j.seq, n.kind, j.op, j.reparented, j.node_id, n.content_hash
           FROM journal j JOIN nodes n ON n.id = j.node_id
          WHERE j.seq > $1 ORDER BY j.seq LIMIT $2",
    )
    .bind(journal_seq)
    .bind(BATCH)
    .fetch_all(&st.db)
    .await?;
    if texts.is_empty() && entries.is_empty() {
        return Ok(false);
    }
    let next_text = texts.last().map_or(text_seq, |t| t.0);
    let next_journal = entries.last().map_or(journal_seq, |e| e.seq);
    let mut hashes: Vec<Vec<u8>> = texts.into_iter().map(|(_, h)| h).collect();
    // A folder moved elsewhere takes its contents along (maybe into "Nur lokal").
    let moved: Vec<i64> = entries
        .iter()
        .filter(|e| e.kind == "dir" && e.op == "move" && e.reparented != Some(false))
        .map(|e| e.node_id)
        .collect();
    hashes.extend(entries.into_iter().filter_map(|e| e.content_hash));
    for dir in moved {
        hashes.extend(super::contents_below(st, dir).await?);
    }
    hashes.sort();
    hashes.dedup();
    for part in hashes.chunks(5000) {
        super::revoke_local(st, part, "Kopie in einem Ordner „Nur lokal“").await?;
        super::queue_texts(st, part).await?;
        super::queue_images(st, part).await?;
    }
    sqlx::query("UPDATE ai_state SET text_seq = $1, journal_seq = $2")
        .bind(next_text)
        .bind(next_journal)
        .execute(&st.db)
        .await?;
    Ok(true)
}

/// What a full check did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Sweep {
    /// Contents whose cloud results were deleted.
    pub revoked: u64,
    /// Contents no file has any more whose results were deleted.
    pub removed: u64,
    /// Jobs added for missing results.
    pub queued: u64,
}

/// The full check: cloud results of contents in "Nur lokal" folders go (also if the default
/// class changed), results of contents no file has any more go, and missing work is queued.
pub async fn sweep(st: &AppState) -> Result<Sweep, ApiError> {
    let mut out = Sweep::default();
    let mut after: Vec<u8> = Vec::new();
    loop {
        // Every cloud result (also a picture description) has its row here.
        let part: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT DISTINCT content_hash FROM ai_done
              WHERE space = 'cloud' AND content_hash > $1 ORDER BY content_hash LIMIT $2",
        )
        .bind(&after)
        .bind(SWEEP_BATCH)
        .fetch_all(&st.db)
        .await?;
        let Some(last) = part.last().cloned() else {
            break;
        };
        out.revoked += super::revoke_local(st, &part, "Prüfung: Kopie in „Nur lokal“").await?;
        after = last;
    }
    out.removed = super::forget_gone(st).await?;
    if st.ai.configured() {
        let mut after: Vec<u8> = Vec::new();
        loop {
            let part: Vec<Vec<u8>> = sqlx::query_scalar(
                "SELECT hash FROM content_text WHERE hash > $1 AND length(text) > 0
                  ORDER BY hash LIMIT $2",
            )
            .bind(&after)
            .bind(SWEEP_BATCH)
            .fetch_all(&st.db)
            .await?;
            let Some(last) = part.last().cloned() else {
                break;
            };
            out.queued += super::queue_texts(st, &part).await?;
            after = last;
        }
        let mut after: Vec<u8> = Vec::new();
        loop {
            let part: Vec<Vec<u8>> = sqlx::query_scalar(
                "SELECT DISTINCT content_hash FROM nodes
                  WHERE content_hash > $1 AND deleted_at IS NULL AND kind = 'file' AND name ~* $2
                  ORDER BY content_hash LIMIT $3",
            )
            .bind(&after)
            .bind(images::NAME_PATTERN)
            .bind(SWEEP_BATCH)
            .fetch_all(&st.db)
            .await?;
            let Some(last) = part.last().cloned() else {
                break;
            };
            out.queued += super::queue_images(st, &part).await?;
            after = last;
        }
    }
    // The search index follows the log within seconds; old entries are no longer needed.
    sqlx::query("DELETE FROM ai_vision_log WHERE at < now() - interval '30 days'")
        .execute(&st.db)
        .await?;
    if out != Sweep::default() {
        tracing::info!(?out, "KI: Prüfung");
    }
    Ok(out)
}

/// Why a job did not get done.
#[derive(Debug)]
enum Fail {
    /// The cloud may not run now (not started, budget spent): the job waits, no attempt counted.
    Paused(&'static str),
    Call(CallError),
    Other(String),
}

impl From<CallError> for Fail {
    fn from(e: CallError) -> Self {
        Fail::Call(e)
    }
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Fail::Other(format!("{e:?}"))
    }
}

impl From<sqlx::Error> for Fail {
    fn from(e: sqlx::Error) -> Self {
        Fail::Other(e.to_string())
    }
}

async fn worker(st: AppState, side: Side, mut stop: watch::Receiver<bool>) {
    let mut failures: u32 = 0;
    loop {
        if *stop.borrow() {
            return;
        }
        // Without a reason to hurry, wait for new work (or the start of the cloud).
        let mut pause = None;
        let paused = match side {
            Side::Cloud => super::cloud_paused(&st, 0.0)
                .await
                .unwrap_or(Some("Datenbank")),
            Side::Local => None,
        };
        if paused.is_none() {
            match jobs::claim(&st.db, &[side.kind()]).await {
                Ok(Some(job)) => {
                    pause = finish(
                        &st,
                        side,
                        &job,
                        handle(&st, side, &job).await,
                        &mut failures,
                    )
                    .await;
                    if pause.is_none() {
                        continue;
                    }
                }
                Ok(None) => {}
                Err(sqlx::Error::PoolClosed) => return,
                Err(e) => tracing::warn!(error = %e, "KI: Aufträge nicht lesbar"),
            }
        }
        match pause {
            // After an error: wait, whatever comes in.
            Some(d) => tokio::select! {
                () = tokio::time::sleep(d) => {}
                _ = stop.changed() => return,
            },
            None => tokio::select! {
                () = st.ai.work.notified() => {}
                () = tokio::time::sleep(IDLE) => {}
                _ = stop.changed() => return,
            },
        }
    }
}

/// Records how a job ended. Returns how long the worker should pause.
async fn finish(
    st: &AppState,
    side: Side,
    job: &Job,
    result: Result<(), Fail>,
    failures: &mut u32,
) -> Option<Duration> {
    let db = &st.db;
    let (r, pause) = match result {
        Ok(()) => {
            *failures = 0;
            st.ai.set_error(side, None);
            (jobs::finish(db, job.id).await, None)
        }
        Err(Fail::Paused(why)) => {
            tracing::debug!(why, "KI: Cloud pausiert");
            (jobs::release(db, job.id).await, Some(IDLE))
        }
        Err(Fail::Call(CallError::Rejected(m))) => {
            tracing::warn!(key = %job.key, error = %m, "KI: Anbieter lehnt den Inhalt ab");
            (jobs::fail(db, job.id, &m).await, None)
        }
        Err(Fail::Call(CallError::Access(m))) => {
            tracing::warn!(error = %m, "KI: Zugang abgelehnt");
            st.ai.set_error(side, Some(m.clone()));
            (jobs::retry(db, job, &m).await, Some(ACCESS_PAUSE))
        }
        Err(Fail::Call(CallError::Transient(m)) | Fail::Other(m)) => {
            tracing::warn!(key = %job.key, attempt = job.attempts, error = %m, "KI: Auftrag fehlgeschlagen");
            st.ai.set_error(side, Some(m.clone()));
            *failures += 1;
            // The provider is unwell: do not run through the whole queue.
            let wait = POLL * 2u32.pow((*failures).min(6));
            (jobs::retry(db, job, &m).await, Some(wait))
        }
    };
    if let Err(e) = r {
        tracing::warn!(key = %job.key, error = %e, "KI: Auftrag nicht abgeschlossen");
    }
    pause
}

/// Does one job (tests call this to work step by step).
pub async fn work_one(st: &AppState, side: Side) -> Result<bool, String> {
    let Some(job) = jobs::claim(&st.db, &[side.kind()])
        .await
        .map_err(|e| e.to_string())?
    else {
        return Ok(false);
    };
    let r = handle(st, side, &job).await;
    let mut failures = 0;
    finish(st, side, &job, r, &mut failures).await;
    Ok(true)
}

async fn handle(st: &AppState, side: Side, job: &Job) -> Result<(), Fail> {
    let Some((task, hex)) = job.key.split_once(':') else {
        return Ok(());
    };
    let Some(hash) = content::unhex(hex) else {
        return Ok(());
    };
    match task {
        "text" => text(st, side, &hash).await,
        "image" => image(st, side, &hash).await,
        _ => Ok(()),
    }
}

/// Embeds the text of a content in the space it belongs to.
async fn text(st: &AppState, side: Side, hash: &[u8; 32]) -> Result<(), Fail> {
    let ai = &st.ai;
    let h = hash.to_vec();
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT seq, text FROM content_text WHERE hash = $1")
            .bind(&h)
            .fetch_optional(&st.db)
            .await?;
    let Some((seq, text)) = row else {
        return Ok(());
    };
    let targets = super::targets(st, std::slice::from_ref(&h)).await?;
    // No file has it any more.
    let Some(space) = targets.get(&h).and_then(|t| ai.text_space(t)) else {
        return Ok(());
    };
    if Side::of(space) != side {
        // The folder's class changed since the job was queued: the other side does it.
        super::queue_texts(st, &[h]).await?;
        return Ok(());
    }
    let Some((model, _)) = ai.model(space) else {
        return Ok(());
    };
    let done: Option<(String, i64)> = sqlx::query_as(
        "SELECT model, version FROM ai_done WHERE content_hash = $1 AND space = $2 AND task = 'text'",
    )
    .bind(&h)
    .bind(space.as_str())
    .fetch_optional(&st.db)
    .await?;
    if done.is_some_and(|(m, v)| m == model && v == seq) {
        return Ok(());
    }
    let max = if space == Space::Cloud {
        CLOUD_CHUNKS
    } else {
        LOCAL_CHUNKS
    };
    let pieces = if chunk::worth_embedding(&text) {
        chunk::split(&text, CHUNK_CHARS, CHUNK_OVERLAP, max)
    } else {
        Vec::new()
    };
    drop(text);
    let inputs: Vec<String> = pieces.iter().map(|c| c.text.clone()).collect();
    let vectors = if inputs.is_empty() {
        Vec::new()
    } else {
        match side {
            Side::Cloud => {
                let cloud = ai.cloud.as_ref().ok_or(Fail::Paused("kein Anbieter"))?;
                let estimate = cloud.embed_cost(estimate_tokens(&inputs));
                if let Some(why) = super::cloud_paused(st, estimate).await? {
                    return Err(Fail::Paused(why));
                }
                let Some(cleared) = super::clear(st, &h).await? else {
                    super::queue_texts(st, &[h]).await?;
                    return Ok(());
                };
                let e = cloud.embed_content(&cleared, &inputs).await?;
                super::record_usage(st, "embed", e.tokens, 0, cloud.embed_cost(e.tokens)).await?;
                e.vectors
            }
            Side::Local => {
                let local = ai.local.as_ref().ok_or(Fail::Paused("kein Dienst"))?;
                local.embed_content(&inputs).await?.vectors
            }
        }
    };
    let pieces: Vec<Piece> = pieces
        .iter()
        .zip(vectors)
        .map(|(c, vec)| Piece {
            start: c.start as i32,
            len: c.len as i32,
            vec,
        })
        .collect();
    let stored = super::store(
        st,
        Outcome {
            hash: &h,
            space,
            model,
            done_as: model,
            version: seq,
            task: "text",
            source: "text",
            pieces,
            vision: None,
        },
    )
    .await?;
    if !stored {
        // Became "Nur lokal" while the cloud worked: the result is thrown away.
        super::queue_texts(st, &[h]).await?;
        return Ok(());
    }
    if side == Side::Cloud && !inputs.is_empty() {
        sqlx::query("UPDATE ai_state SET trial_left = trial_left - 1 WHERE trial_left > 0")
            .execute(&st.db)
            .await?;
    }
    Ok(())
}

/// A stored picture description.
#[derive(sqlx::FromRow)]
struct KnownVision {
    description: String,
    tags: Vec<String>,
    text_in_image: String,
    doc_type: Option<String>,
    date_found: Option<time::Date>,
}

/// Analyses a picture: in the cloud a description (embedded as text and indexed for the lexical
/// search), in the home network a CLIP vector.
async fn image(st: &AppState, side: Side, hash: &[u8; 32]) -> Result<(), Fail> {
    let ai = &st.ai;
    let h = hash.to_vec();
    let targets = super::targets(st, std::slice::from_ref(&h)).await?;
    let Some(space) = targets.get(&h).and_then(|t| ai.image_space(t)) else {
        return Ok(());
    };
    if Side::of(space) != side {
        super::queue_images(st, &[h]).await?;
        return Ok(());
    }
    let Some(done_as) = ai.image_done_as(space) else {
        return Ok(());
    };
    let done: Option<String> = sqlx::query_scalar(
        "SELECT model FROM ai_done WHERE content_hash = $1 AND space = $2 AND task = 'image'",
    )
    .bind(&h)
    .bind(space.as_str())
    .fetch_optional(&st.db)
    .await?;
    if done.as_deref() == Some(done_as.as_str()) {
        return Ok(());
    }
    let mut outcome = Outcome {
        hash: &h,
        space,
        model: "",
        done_as: &done_as,
        version: 0,
        task: "image",
        source: "image",
        pieces: Vec::new(),
        vision: None,
    };
    let seen: Option<Seen>;
    let mut called = false;
    match space {
        Space::Cloud => {
            let cloud = ai.cloud.as_ref().ok_or(Fail::Paused("kein Anbieter"))?;
            let vision = cloud
                .vision
                .as_ref()
                .ok_or(Fail::Paused("kein Bildmodell"))?;
            outcome.model = &cloud.embed.model;
            outcome.source = "description";
            // A description by the same model is kept (e.g. when only the text model changed).
            let known: Option<KnownVision> = sqlx::query_as(
                "SELECT description, tags, text_in_image, doc_type, date_found FROM ai_vision
                  WHERE content_hash = $1 AND model = $2",
            )
            .bind(&h)
            .bind(&vision.model)
            .fetch_optional(&st.db)
            .await?;
            seen = match known {
                Some(k) => Some(Seen {
                    description: k.description,
                    tags: k.tags,
                    text: k.text_in_image,
                    doc_type: k.doc_type,
                    date: k.date_found,
                    ..Seen::default()
                }),
                None => {
                    // A picture costs a few hundred tokens in, its description some out.
                    let estimate = cloud.vision_cost(1500, 400) + cloud.embed_cost(600);
                    if let Some(why) = super::cloud_paused(st, estimate).await? {
                        return Err(Fail::Paused(why));
                    }
                    let Some(cleared) = super::clear(st, &h).await? else {
                        super::queue_images(st, &[h]).await?;
                        return Ok(());
                    };
                    match images::read(st, hash).await.map_err(Fail::Other)? {
                        Read::Gone => return Ok(()),
                        Read::Unusable => None,
                        Read::Picture(p) => {
                            let s = cloud.describe(&cleared, &p).await?;
                            called = true;
                            super::record_usage(
                                st,
                                "vision",
                                s.tokens_in,
                                s.tokens_out,
                                cloud.vision_cost(s.tokens_in, s.tokens_out),
                            )
                            .await?;
                            Some(s)
                        }
                    }
                }
            };
            if let Some(s) = &seen {
                let words = s.words();
                if chunk::worth_embedding(&words) {
                    if let Some(why) = super::cloud_paused(
                        st,
                        cloud.embed_cost(estimate_tokens(std::slice::from_ref(&words))),
                    )
                    .await?
                    {
                        return Err(Fail::Paused(why));
                    }
                    let Some(cleared) = super::clear(st, &h).await? else {
                        super::queue_images(st, &[h]).await?;
                        return Ok(());
                    };
                    let e = cloud.embed_content(&cleared, &[words]).await?;
                    called = true;
                    super::record_usage(st, "embed", e.tokens, 0, cloud.embed_cost(e.tokens))
                        .await?;
                    outcome.pieces = e
                        .vectors
                        .into_iter()
                        .map(|vec| Piece {
                            start: 0,
                            len: 0,
                            vec,
                        })
                        .collect();
                }
            }
            outcome.vision = seen.as_ref().map(|s| (vision.model.as_str(), s));
        }
        Space::Clip | Space::Local => {
            let local = ai.local.as_ref().ok_or(Fail::Paused("kein Dienst"))?;
            let clip = local.clip.as_ref().ok_or(Fail::Paused("kein Bildmodell"))?;
            outcome.model = &clip.model;
            match images::read(st, hash).await.map_err(Fail::Other)? {
                Read::Gone => return Ok(()),
                Read::Unusable => {}
                Read::Picture(p) => {
                    let vec = local.embed_picture(&p).await?;
                    outcome.pieces = vec![Piece {
                        start: 0,
                        len: 0,
                        vec,
                    }];
                }
            }
        }
    }
    if !super::store(st, outcome).await? {
        // Became "Nur lokal" while the cloud worked: the result is thrown away.
        super::queue_images(st, &[h]).await?;
        return Ok(());
    }
    if side == Side::Cloud && called {
        sqlx::query("UPDATE ai_state SET trial_left = trial_left - 1 WHERE trial_left > 0")
            .execute(&st.db)
            .await?;
    }
    Ok(())
}

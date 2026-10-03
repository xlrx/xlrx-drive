//! Outside cache (PLAN 15.2): selected contents of "Cloud erlaubt" folders are mirrored to a
//! private S3 bucket, and people outside the home network get them from there (a `307` to a
//! presigned URL) instead of through the home line and the NAS CPU.
//!
//! - **What:** files behind public links (from when the link is made), contents fetched from
//!   outside repeatedly, and for people who asked for it, their starred and suggested files
//!   (at night).
//! - **Never:** a content that also lies in a "Nur lokal" folder – as a file, an old version or in
//!   the trash. Checked when planning, right before and after uploading, before every redirect, and
//!   every minute for everything in the bucket; making a folder "Nur lokal" removes its objects at
//!   once.
//! - **Content-addressed:** one object per content, under a random name: the bucket sees neither
//!   file names nor hashes that could be matched with known files, and an object uploaded again
//!   gets a new name, so presigned URLs handed out before never work again. A new version is a
//!   new object; the old one goes when no file has that content any more.
//! - **Cleanup:** a link's objects go when no usable link points at them any more; otherwise least
//!   recently used within a size budget, and after `XLRX_S3_MAX_DAYS` days without use.
//! - **Inside the home network** nothing is ever redirected.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::time::Duration;

use super::data_class::{self, Class};
use super::db::{self, NODE_COLS, NodeRow};
use super::links::{self, LINK_COLS, LinkRow};
use super::{content, roots};
use crate::error::{ApiError, ApiResult};
use crate::s3::{S3, Throttle};
use crate::state::AppState;

/// How long a presigned URL works: a download starts at once; media are played for a while.
const DOWNLOAD_URL: Duration = Duration::from_secs(10 * 60);
const MEDIA_URL: Duration = Duration::from_secs(60 * 60);
/// Fetched from outside this often within a week: worth mirroring.
const POPULAR_FETCHES: i64 = 2;
/// Suggestions prefetched per person.
const PREFETCH_SUGGESTIONS: usize = 8;
/// Without a wake-up, the cache is looked after this often.
const TICK: Duration = Duration::from_secs(60);

/// A network in CIDR notation (`2001:db8:1::/48`, `203.0.113.0/24`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr {
    net: IpAddr,
    len: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (addr, len) = s.split_once('/').unwrap_or((s, ""));
        let net: IpAddr = addr
            .trim()
            .parse()
            .map_err(|_| format!("„{s}“ ist kein Netz"))?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let len = if len.is_empty() {
            max
        } else {
            len.trim()
                .parse()
                .map_err(|_| format!("„{s}“: ungültige Präfixlänge"))?
        };
        if len > max {
            return Err(format!("„{s}“: Präfix länger als {max}"));
        }
        Ok(Self { net, len })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        let (a, b, bits) = match (self.net, ip) {
            (IpAddr::V4(n), IpAddr::V4(i)) => (u32::from(n) as u128, u32::from(i) as u128, 32),
            (IpAddr::V6(n), IpAddr::V6(i)) => (u128::from(n), u128::from(i), 128),
            _ => return false,
        };
        let shift = bits - u32::from(self.len);
        shift >= bits || (a >> shift) == (b >> shift)
    }
}

/// Whether a request comes from outside the home network. Unknown addresses count as inside:
/// in doubt, the NAS serves.
pub fn outside(st: &AppState, ip: &str) -> bool {
    let Ok(ip) = ip.parse::<IpAddr>() else {
        return false;
    };
    !(crate::extract::tika::is_local(ip) || st.cfg.lan_nets.iter().any(|n| n.contains(ip)))
}

/// A fresh object key (random: says nothing about the content, and differs for every upload).
fn new_key() -> String {
    let b: [u8; 16] = crate::auth::tokens::random_bytes();
    format!(
        "x/{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// Contents that must not be (or stay) in the bucket: a copy that still exists – a file, one in
/// the trash, an old version – lies in a "Nur lokal" folder, or no live file in a "Cloud erlaubt"
/// folder has it.
pub async fn forbidden(st: &AppState, hashes: &[Vec<u8>]) -> ApiResult<HashSet<Vec<u8>>> {
    if hashes.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT n.id, n.content_hash, n.deleted_at IS NULL FROM nodes n
          WHERE n.content_hash = ANY($1)
            AND (n.deleted_at IS NULL OR EXISTS (
                  SELECT 1 FROM nodes t WHERE t.id = n.deleted_with AND t.trash_path IS NOT NULL))
         UNION ALL
         SELECT node_id, content_hash, false FROM versions WHERE content_hash = ANY($1)",
    )
    .bind(hashes)
    .fetch_all(&st.db)
    .await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.0)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let classes = data_class::effective(&st.db, st.cfg.default_data_class, &ids).await?;
    let mut local = HashSet::new();
    let mut allowed = HashSet::new();
    for (id, hash, live) in rows {
        // Unknown means local: only an explicit "Cloud erlaubt" lets a content out.
        match classes.get(&id).map(|e| e.class) {
            Some(Class::Cloud) if live => {
                allowed.insert(hash);
            }
            Some(Class::Cloud) => {}
            _ => {
                local.insert(hash);
            }
        }
    }
    Ok(hashes
        .iter()
        .filter(|h| local.contains(*h) || !allowed.contains(*h))
        .cloned()
        .collect())
}

/// Marks objects whose content may no longer be outside for deletion (also before the cache
/// worker gets to them: nothing marked is ever served again). Returns how many.
pub async fn revoke_forbidden(st: &AppState) -> ApiResult<u64> {
    if st.s3.is_none() {
        return Ok(0);
    }
    let hashes: Vec<Vec<u8>> =
        sqlx::query_scalar("SELECT content_hash FROM s3_objects WHERE state <> 'deleting'")
            .fetch_all(&st.db)
            .await?;
    let bad: Vec<Vec<u8>> = forbidden(st, &hashes).await?.into_iter().collect();
    let n = mark_deleting(st, &bad).await?;
    if n > 0 {
        tracing::info!(
            objects = n,
            "Außen-Cache: nicht mehr erlaubte Inhalte entfernt"
        );
        st.mirror_wake.notify_one();
    }
    Ok(n)
}

async fn mark_deleting(st: &AppState, hashes: &[Vec<u8>]) -> ApiResult<u64> {
    if hashes.is_empty() {
        return Ok(0);
    }
    Ok(sqlx::query(
        "UPDATE s3_objects SET state = 'deleting' WHERE content_hash = ANY($1) AND state <> 'deleting'",
    )
    .bind(hashes)
    .execute(&st.db)
    .await?
    .rows_affected())
}

/// Contents public links serve: files behind usable links (not "Nur hochladen"), big enough.
async fn link_contents(st: &AppState) -> ApiResult<HashSet<Vec<u8>>> {
    let rows: Vec<LinkRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LINK_COLS} FROM links
          WHERE kind <> 'upload' AND (expires_at IS NULL OR expires_at > now())"
    )))
    .fetch_all(&st.db)
    .await?;
    let mut out = HashSet::new();
    for row in rows {
        let Ok(open) = links::usable(&st.db, row).await else {
            continue;
        };
        if let (false, Some(h), Some(size)) =
            (open.node.is_dir(), open.node.content_hash, open.node.size)
        {
            if size as u64 >= st.cfg.mirror.min_size {
                out.insert(h);
            }
        }
    }
    Ok(out)
}

#[derive(Clone, Debug)]
struct Want {
    hash: Vec<u8>,
    reason: &'static str,
}

/// What should be in the bucket and is not, most important first.
async fn wanted(st: &AppState) -> ApiResult<Vec<Want>> {
    let mut out: Vec<Want> = link_contents(st)
        .await?
        .into_iter()
        .map(|hash| Want {
            hash,
            reason: "link",
        })
        .collect();
    let popular: Vec<Vec<u8>> = sqlx::query_scalar(
        "SELECT content_hash FROM remote_fetches WHERE at > now() - interval '7 days'
          GROUP BY content_hash HAVING count(*) >= $1 ORDER BY max(at) DESC",
    )
    .bind(POPULAR_FETCHES)
    .fetch_all(&st.db)
    .await?;
    out.extend(popular.into_iter().map(|hash| Want {
        hash,
        reason: "popular",
    }));
    if prefetch_time(st).await? {
        out.extend(prefetch(st).await?.into_iter().map(|hash| Want {
            hash,
            reason: "prefetch",
        }));
    }
    let present: HashSet<Vec<u8>> = sqlx::query_scalar("SELECT content_hash FROM s3_objects")
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .collect();
    let mut seen = HashSet::new();
    out.retain(|w| !present.contains(&w.hash) && seen.insert(w.hash.clone()));
    Ok(out)
}

async fn prefetch_time(st: &AppState) -> ApiResult<bool> {
    let (from, to) = st.cfg.mirror.prefetch_hours;
    let hour: i32 = sqlx::query_scalar("SELECT extract(hour FROM now() AT TIME ZONE $1)::int")
        .bind(&st.cfg.timezone)
        .fetch_one(&st.db)
        .await?;
    let hour = hour as u32;
    Ok(if from <= to {
        hour >= from && hour < to
    } else {
        hour >= from || hour < to
    })
}

/// For people who asked for it: their starred files and their best suggestions.
async fn prefetch(st: &AppState) -> ApiResult<Vec<Vec<u8>>> {
    let users: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM users WHERE prefetch AND disabled_at IS NULL ORDER BY id",
    )
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::new();
    for user in users {
        let starred: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT {} FROM stars s JOIN nodes n ON n.id = s.node_id
              WHERE s.user_id = $1 AND n.deleted_at IS NULL AND n.kind = 'file'
              ORDER BY s.created_at DESC LIMIT 50",
            NODE_COLS
                .split(',')
                .map(|c| format!("n.{}", c.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        )))
        .bind(user)
        .fetch_all(&st.db)
        .await?;
        let roles = super::access::roles(&st.db, user, &starred).await?;
        let mut nodes: Vec<NodeRow> = starred
            .into_iter()
            .filter(|n| roles.contains_key(&n.id))
            .collect();
        nodes.extend(
            crate::api::suggest::ranked(st, user)
                .await?
                .into_iter()
                .map(|r| r.2)
                .filter(|n| !n.is_dir())
                .take(PREFETCH_SUGGESTIONS),
        );
        out.extend(nodes.into_iter().filter_map(|n| {
            (n.size.unwrap_or(0) as u64 >= st.cfg.mirror.min_size)
                .then_some(n.content_hash)
                .flatten()
        }));
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct Object {
    content_hash: Vec<u8>,
    size: i64,
    state: String,
    reason: String,
    /// Not used for longer than allowed.
    old: bool,
}

/// Decides what goes: no longer allowed, link ended, too old, over budget, interrupted uploads;
/// then removes it from the bucket.
pub async fn sweep(st: &AppState) -> ApiResult<()> {
    let Some(s3) = st.s3.clone() else {
        return Ok(());
    };
    let objects: Vec<Object> = sqlx::query_as(
        "SELECT content_hash, size, state, reason,
                coalesce(coalesce(last_hit, uploaded_at) < now() - make_interval(days => $1), false)
                  AS old
           FROM s3_objects
          ORDER BY coalesce(last_hit, uploaded_at, created_at)",
    )
    .bind(st.cfg.mirror.max_days as i32)
    .fetch_all(&st.db)
    .await?;
    let all: Vec<Vec<u8>> = objects.iter().map(|o| o.content_hash.clone()).collect();
    let bad = forbidden(st, &all).await?;
    let for_links = link_contents(st).await?;
    let mut gone: Vec<Vec<u8>> = Vec::new();
    let mut used: u64 = 0;
    for o in &objects {
        let held = for_links.contains(&o.content_hash);
        let goes = o.state == "deleting"
            // Only the worker uploads, one at a time, and not while sweeping: an upload still
            // marked here was interrupted.
            || o.state == "uploading"
            || bad.contains(&o.content_hash)
            || (o.reason == "link" && !held)
            || (o.old && !held);
        if goes {
            gone.push(o.content_hash.clone());
        } else {
            used += o.size as u64;
        }
    }
    // Over budget: least recently used first, objects held by a link last.
    if used > st.cfg.mirror.budget {
        let mut by_use: Vec<&Object> = objects
            .iter()
            .filter(|o| !gone.contains(&o.content_hash))
            .collect();
        by_use.sort_by_key(|o| for_links.contains(&o.content_hash));
        for o in by_use {
            if used <= st.cfg.mirror.budget {
                break;
            }
            used -= o.size as u64;
            gone.push(o.content_hash.clone());
        }
    }
    mark_deleting(st, &gone).await?;
    let doomed: Vec<(Vec<u8>, String)> =
        sqlx::query_as("SELECT content_hash, key FROM s3_objects WHERE state = 'deleting'")
            .fetch_all(&st.db)
            .await?;
    for (hash, key) in doomed {
        match s3.delete(&key).await {
            Ok(()) => {
                sqlx::query(
                    "DELETE FROM s3_objects WHERE content_hash = $1 AND state = 'deleting'",
                )
                .bind(&hash)
                .execute(&st.db)
                .await?;
            }
            Err(e) => return Err(ApiError::Unavailable(e.to_string())),
        }
    }
    Ok(())
}

/// Removes objects in the bucket the database does not know (an upload finished just before a
/// crash). Only below the cache's own prefix.
pub async fn orphans(st: &AppState) -> ApiResult<u64> {
    let Some(s3) = st.s3.clone() else {
        return Ok(0);
    };
    let listed = s3
        .list("x/")
        .await
        .map_err(|e| ApiError::Unavailable(e.to_string()))?;
    let known: HashSet<String> = sqlx::query_scalar("SELECT key FROM s3_objects")
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .collect();
    let mut n = 0;
    for (key, _) in listed {
        if !known.contains(&key) {
            s3.delete(&key)
                .await
                .map_err(|e| ApiError::Unavailable(e.to_string()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// Uploads one content. `false`: nothing to do (not allowed, over budget, file changed).
async fn upload(st: &AppState, s3: &S3, w: &Want, throttle: &mut Throttle) -> ApiResult<bool> {
    if !forbidden(st, std::slice::from_ref(&w.hash))
        .await?
        .is_empty()
    {
        return Ok(false);
    }
    let node: Option<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes
          WHERE content_hash = $1 AND deleted_at IS NULL AND kind = 'file' LIMIT 1"
    )))
    .bind(&w.hash)
    .fetch_optional(&st.db)
    .await?;
    let Some(node) = node else { return Ok(false) };
    let size = node.size.unwrap_or(0).max(0) as u64;
    let used: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(size), 0)::bigint FROM s3_objects WHERE state <> 'deleting'",
    )
    .fetch_one(&st.db)
    .await?;
    if used as u64 + size > st.cfg.mirror.budget {
        if w.reason != "link" {
            return Ok(false);
        }
        // A link's file makes room: least recently used objects not held by a link go.
        let for_links = link_contents(st).await?;
        let candidates: Vec<(Vec<u8>, i64)> = sqlx::query_as(
            "SELECT content_hash, size FROM s3_objects WHERE state = 'ready'
              ORDER BY coalesce(last_hit, uploaded_at, created_at)",
        )
        .fetch_all(&st.db)
        .await?;
        let mut free = st.cfg.mirror.budget as i64 - used;
        let mut gone = Vec::new();
        for (h, s) in candidates {
            if free >= size as i64 {
                break;
            }
            if !for_links.contains(&h) {
                free += s;
                gone.push(h);
            }
        }
        if free < size as i64 {
            return Ok(false);
        }
        mark_deleting(st, &gone).await?;
    }
    let hash: [u8; 32] = w
        .hash
        .as_slice()
        .try_into()
        .map_err(|_| ApiError::Internal("Inhalts-Hash mit falscher Länge".into()))?;
    let key = new_key();
    let added = sqlx::query(
        "INSERT INTO s3_objects (content_hash, key, size, state, reason)
         VALUES ($1, $2, $3, 'uploading', $4) ON CONFLICT DO NOTHING",
    )
    .bind(&w.hash)
    .bind(&key)
    .bind(size as i64)
    .bind(w.reason)
    .execute(&st.db)
    .await?;
    if added.rows_affected() == 0 {
        return Ok(false);
    }
    // A private snapshot with exactly this content: the file may change meanwhile.
    let root = db::root_by_id(&st.db, node.root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let data_dir = st.cfg.data_dir.as_deref().ok_or(ApiError::NotFound)?;
    let path = roots::dir(data_dir, &root).join(db::rel_path(&st.db, node.id).await?);
    let Some(snapshot) = content::stage_from(st, &path, &hash).await? else {
        sqlx::query("DELETE FROM s3_objects WHERE content_hash = $1 AND state = 'uploading'")
            .bind(&w.hash)
            .execute(&st.db)
            .await?;
        return Ok(false);
    };
    if let Err(e) = s3.put_file(&key, snapshot.path(), throttle).await {
        mark_deleting(st, std::slice::from_ref(&w.hash)).await?;
        return Err(ApiError::Unavailable(e.to_string()));
    }
    drop(snapshot);
    // Still allowed (a copy may have been put into a "Nur lokal" folder meanwhile), and not
    // revoked meanwhile: only then may it be served.
    if !forbidden(st, std::slice::from_ref(&w.hash))
        .await?
        .is_empty()
    {
        mark_deleting(st, std::slice::from_ref(&w.hash)).await?;
        return Ok(false);
    }
    let ready = sqlx::query(
        "UPDATE s3_objects SET state = 'ready', uploaded_at = now()
          WHERE content_hash = $1 AND state = 'uploading'",
    )
    .bind(&w.hash)
    .execute(&st.db)
    .await?;
    tracing::info!(reason = w.reason, size, "Außen-Cache: Inhalt gespiegelt");
    Ok(ready.rows_affected() == 1)
}

/// What one round did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    pub uploaded: usize,
}

/// One round: sweep, then upload what is wanted (one at a time). The worker runs this every
/// minute or when woken; tests call it directly.
pub async fn round(
    st: &AppState,
    backoff: &mut HashMap<Vec<u8>, (tokio::time::Instant, u32)>,
) -> ApiResult<Round> {
    let Some(s3) = st.s3.clone() else {
        return Ok(Round::default());
    };
    sweep(st).await?;
    let mut done = Round::default();
    let now = tokio::time::Instant::now();
    for w in wanted(st).await? {
        if backoff.get(&w.hash).is_some_and(|(until, _)| *until > now) {
            continue;
        }
        let mut throttle = Throttle::new(st.cfg.mirror.upload_rate);
        match upload(st, &s3, &w, &mut throttle).await {
            Ok(true) => {
                done.uploaded += 1;
                backoff.remove(&w.hash);
            }
            Ok(false) => {}
            Err(e) => {
                let n = backoff.get(&w.hash).map(|b| b.1 + 1).unwrap_or(0);
                let wait = Duration::from_secs(300 * 2u64.pow(n.min(6)));
                backoff.insert(w.hash.clone(), (tokio::time::Instant::now() + wait, n));
                return Err(e);
            }
        }
        // Whatever happened meanwhile (a link ended, a folder became "Nur lokal") is handled
        // before the next upload.
        sweep(st).await?;
    }
    Ok(done)
}

/// Starts the cache worker (if a bucket is configured).
pub fn start(st: &AppState) {
    if st.s3.is_none() {
        return;
    }
    let st = st.clone();
    tokio::spawn(async move {
        let mut backoff = HashMap::new();
        let mut last_orphans: Option<tokio::time::Instant> = None;
        loop {
            let r = round(&st, &mut backoff).await;
            if last_orphans.is_none_or(|t| t.elapsed() > Duration::from_secs(3600)) {
                last_orphans = Some(tokio::time::Instant::now());
                if let Err(e) = orphans(&st).await {
                    tracing::warn!(error = ?e, "Außen-Cache: Aufräumen im Bucket fehlgeschlagen");
                }
            }
            *st.mirror_error.lock().expect("mutex") = r.err().map(|e| {
                tracing::warn!(error = ?e, "Außen-Cache");
                match e {
                    ApiError::Unavailable(m) | ApiError::Internal(m) => m,
                    other => format!("{other:?}"),
                }
            });
            tokio::select! {
                () = st.mirror_wake.notified() => {}
                () = tokio::time::sleep(TICK) => {}
            }
        }
    });
}

/// Notes a full download from outside (what is fetched repeatedly gets mirrored).
pub async fn fetched(st: &AppState, ip: &str, node: &NodeRow) -> ApiResult<()> {
    if st.s3.is_none() || !outside(st, ip) {
        return Ok(());
    }
    if let Some(h) = &node.content_hash {
        sqlx::query("INSERT INTO remote_fetches (content_hash) VALUES ($1)")
            .bind(h)
            .execute(&st.db)
            .await?;
    }
    Ok(())
}

/// Where someone outside the home network gets this file from the bucket, if it is there and
/// may be there. Downloads of any type; shown in the browser only audio and video (other
/// previews are small and come from the NAS).
pub async fn redirect(st: &AppState, ip: &str, node: &NodeRow, inline: bool) -> Option<String> {
    let s3 = st.s3.as_ref()?;
    if !outside(st, ip) {
        return None;
    }
    let hash = node.content_hash.clone()?;
    let mime = crate::api::files::mime_of(&node.name);
    let (kind, content_type, valid) = if !inline {
        (
            "attachment",
            "application/octet-stream".to_owned(),
            DOWNLOAD_URL,
        )
    } else if mime.starts_with("video/") || mime.starts_with("audio/") {
        ("inline", mime, MEDIA_URL)
    } else {
        return None;
    };
    let key: Option<String> = match sqlx::query_scalar(
        "SELECT key FROM s3_objects WHERE content_hash = $1 AND state = 'ready'",
    )
    .bind(&hash)
    .fetch_optional(&st.db)
    .await
    {
        Ok(k) => k,
        Err(e) => {
            tracing::warn!(error = %e, "Außen-Cache nicht lesbar");
            return None;
        }
    };
    let key = key?;
    // Checked again for every request: nothing that became "Nur lokal" is ever handed out.
    match forbidden(st, std::slice::from_ref(&hash)).await {
        Ok(bad) if bad.is_empty() => {}
        Ok(_) => {
            let _ = mark_deleting(st, std::slice::from_ref(&hash)).await;
            st.mirror_wake.notify_one();
            return None;
        }
        Err(_) => return None,
    }
    let _ = sqlx::query(
        "UPDATE s3_objects SET hits = hits + 1, last_hit = now() WHERE content_hash = $1",
    )
    .bind(&hash)
    .execute(&st.db)
    .await;
    let disposition = crate::api::files::disposition(kind, &node.name);
    Some(s3.presign_get(
        &key,
        valid,
        disposition.to_str().unwrap_or(kind),
        &content_type,
    ))
}

#[derive(serde::Serialize)]
pub struct Status {
    pub configured: bool,
    pub origin: Option<String>,
    pub bucket: Option<String>,
    pub objects: i64,
    pub bytes: i64,
    pub budget: u64,
    pub uploading: i64,
    pub by_reason: HashMap<String, i64>,
    pub hits: i64,
    pub error: Option<String>,
}

pub async fn status(st: &AppState) -> ApiResult<Status> {
    let rows: Vec<(String, String, i64, i64, i64)> = sqlx::query_as(
        "SELECT state, reason, count(*), coalesce(sum(size), 0)::bigint, coalesce(sum(hits), 0)::bigint
           FROM s3_objects GROUP BY state, reason",
    )
    .fetch_all(&st.db)
    .await?;
    let mut s = Status {
        configured: st.s3.is_some(),
        origin: st.cfg.s3.as_ref().map(|c| c.public_origin()),
        bucket: st.cfg.s3.as_ref().map(|c| c.bucket.clone()),
        objects: 0,
        bytes: 0,
        budget: st.cfg.mirror.budget,
        uploading: 0,
        by_reason: HashMap::new(),
        hits: 0,
        error: st.mirror_error.lock().expect("mutex").clone(),
    };
    for (state, reason, n, bytes, hits) in rows {
        match state.as_str() {
            "ready" => {
                s.objects += n;
                s.bytes += bytes;
                s.hits += hits;
                *s.by_reason.entry(reason).or_default() += n;
            }
            "uploading" => s.uploading += n,
            _ => {}
        }
    }
    Ok(s)
}

/// Empties the cache (everything is removed from the bucket by the worker).
pub async fn clear(st: &AppState) -> ApiResult<u64> {
    let n = sqlx::query("UPDATE s3_objects SET state = 'deleting' WHERE state <> 'deleting'")
        .execute(&st.db)
        .await?
        .rows_affected();
    st.mirror_wake.notify_one();
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks() {
        let n = Cidr::parse("2001:db8:1::/48").unwrap();
        assert!(n.contains("2001:db8:1:ff::7".parse().unwrap()));
        assert!(!n.contains("2001:db8:2::7".parse().unwrap()));
        assert!(!n.contains("192.0.2.1".parse().unwrap()));
        let v4 = Cidr::parse("203.0.113.0/24").unwrap();
        assert!(v4.contains("203.0.113.200".parse().unwrap()));
        assert!(v4.contains("::ffff:203.0.113.9".parse().unwrap()));
        assert!(!v4.contains("203.0.114.1".parse().unwrap()));
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert_eq!(
            Cidr::parse("192.0.2.7").unwrap(),
            Cidr::parse("192.0.2.7/32").unwrap()
        );
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("drive.example.de/24").is_err());
    }
}

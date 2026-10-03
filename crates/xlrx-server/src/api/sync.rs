//! Sync API (PLAN 5.2): the change feed of a root and live notifications.

use std::convert::Infallible;
use std::path::PathBuf;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Rev, Seq};
use xlrx_sync::{Reject, RemoteEntry, RemoteOp, RemoteResult};

use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::access;
use crate::files::content::{self, Staged, Target, Written};
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::files::{live, ops, roots, store};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ChangesQuery {
    pub root: i64,
    /// Last cursor received; 0 (or none) for the first fetch.
    #[serde(default)]
    pub cursor: i64,
    pub limit: Option<i64>,
}

/// One node's current state; `None` if it was deleted (or is no longer visible).
#[derive(Serialize)]
pub struct Change {
    pub node: NodeId,
    pub state: Option<RemoteEntry>,
}

#[derive(Serialize)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub cursor: i64,
    /// More changes follow: fetch them with `cursor` and apply all pages together. (A page may
    /// contain a node whose folder only comes on a later page.)
    pub more: bool,
}

fn entry(n: &NodeRow) -> Option<RemoteEntry> {
    let parent = n.parent_id?;
    let content = match (&n.content_hash, n.size) {
        (Some(h), Some(size)) => Some(FileContent {
            hash: ContentHash(h.as_slice().try_into().ok()?),
            size: size as u64,
        }),
        _ => None,
    };
    Some(RemoteEntry {
        parent: NodeId(parent as u64),
        name: Name::new(&n.name).ok()?,
        kind: if n.is_dir() { Kind::Dir } else { Kind::File },
        content,
        rev: Rev(n.rev as u64),
    })
}

/// Changes of a root since a cursor: the current state of every node changed since then, in
/// journal order. The first fetch (cursor 0) lists the live nodes only.
pub async fn changes(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<Changes>> {
    let root = db::root_by_id(&st.db, q.root)
        .await?
        .ok_or(ApiError::NotFound)?;
    if roots::role(&st, &root, me.id).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let limit = q.limit.unwrap_or(1000).clamp(1, 10_000);
    // One query, one snapshot: writers commit in sequence order (journal lock), so nothing with
    // a lower sequence can appear later.
    let mut rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes
         WHERE root_id = $1 AND seq > $2 AND parent_id IS NOT NULL
           AND ($2 > 0 OR deleted_at IS NULL)
         ORDER BY seq LIMIT $3"
    )))
    .bind(root.id)
    .bind(q.cursor)
    .bind(limit + 1)
    .fetch_all(&st.db)
    .await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let cursor = rows.last().map_or(q.cursor, |n| n.seq);
    let changes = rows
        .iter()
        .map(|n| Change {
            node: NodeId(n.id as u64),
            state: if n.deleted_at.is_some() {
                None
            } else {
                entry(n)
            },
        })
        .collect();
    Ok(Json(Changes {
        changes,
        cursor,
        more,
    }))
}

#[derive(Serialize)]
struct Changed {
    root: i64,
    seq: i64,
}

/// Server-sent events: `change` with `[{"root": …, "seq": …}]` whenever something in a root the
/// person can see changed. Clients then fetch the changes (or reload the view).
pub async fn notify(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let rx = live::subscribe(&st).await?;
    let bell = st.bell.subscribe();
    let last = *rx.borrow();
    let user = me.id;
    let stream = futures_util::stream::unfold(
        (st, rx, bell, last),
        move |(st, mut rx, mut bell, mut last)| async move {
            loop {
                tokio::select! {
                    r = rx.changed() => r.ok()?,
                    b = bell.recv() => {
                        match b {
                            Ok(u) if u != user => continue,
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                            // Mine, or missed some: tell the current count.
                            _ => {}
                        }
                        let unread = crate::bell::unread(&st.db, user).await.ok()?;
                        let event = Event::default()
                            .event("notification")
                            .json_data(serde_json::json!({ "unread": unread }))
                            .unwrap_or_default();
                        return Some((Ok(event), (st, rx, bell, last)));
                    }
                }
                let now = *rx.borrow_and_update();
                // Looked up each time: a root created (or shared) after connecting counts too.
                let scope = access::scope(&st.db, user).await.ok()?;
                let mut rows: Vec<(i64, i64)> = sqlx::query_as(
                    "SELECT root_id, max(seq) FROM journal
                     WHERE seq > $1 AND seq <= $2 AND root_id = ANY($3) GROUP BY root_id",
                )
                .bind(last)
                .bind(now)
                .bind(&scope.roots)
                .fetch_all(&st.db)
                .await
                .ok()?;
                // In other people's roots only what happened inside the items shared with the
                // person: nothing else there is any of their business.
                if !scope.shared.is_empty() {
                    let shared: Vec<(i64, i64)> = sqlx::query_as(
                        "WITH RECURSIVE j AS (
                            SELECT seq, root_id, node_id FROM journal
                             WHERE seq > $1 AND seq <= $2 AND NOT (root_id = ANY($3))
                         ), up AS (
                            SELECT j.seq, j.root_id, n.id, n.parent_id, 0 AS depth
                              FROM j JOIN nodes n ON n.id = j.node_id
                            UNION ALL
                            SELECT up.seq, up.root_id, p.id, p.parent_id, up.depth + 1
                              FROM up JOIN nodes p ON p.id = up.parent_id WHERE up.depth < 1000
                         )
                         SELECT root_id, max(seq) FROM up WHERE id = ANY($4) GROUP BY root_id",
                    )
                    .bind(last)
                    .bind(now)
                    .bind(&scope.roots)
                    .bind(&scope.shared)
                    .fetch_all(&st.db)
                    .await
                    .ok()?;
                    rows.extend(shared);
                }
                last = now;
                if rows.is_empty() {
                    continue;
                }
                let data: Vec<Changed> = rows
                    .into_iter()
                    .map(|(root, seq)| Changed { root, seq })
                    .collect();
                let event = Event::default()
                    .event("change")
                    .json_data(data)
                    .unwrap_or_default();
                return Some((Ok(event), (st, rx, bell, last)));
            }
        },
    );
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

pub(crate) fn hex32(s: &str) -> ApiResult<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return Err(ApiError::bad("Ungültiger Hash."));
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = s
            .get(i * 2..i * 2 + 2)
            .and_then(|x| u8::from_str_radix(x, 16).ok())
            .ok_or_else(|| ApiError::bad("Ungültiger Hash."))?;
    }
    Ok(out)
}

pub(crate) fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn state_dir(st: &AppState) -> ApiResult<PathBuf> {
    st.cfg
        .state_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))
}

/// Content uploaded by this person ahead of an operation (`PUT /sync/content/{hash}`).
pub(crate) fn uploaded_path(st: &AppState, user: i64, hash: &[u8; 32]) -> ApiResult<PathBuf> {
    Ok(state_dir(st)?
        .join(store::STAGING)
        .join(format!("content-{user}-{}", hex(hash))))
}

/// Files and versions the person can read that should have this content.
async fn holders(st: &AppState, user: i64, hash: &[u8; 32]) -> ApiResult<Vec<PathBuf>> {
    let visible: Vec<i64> = roots::readable(st, user)
        .await?
        .iter()
        .map(|r| r.id)
        .collect();
    let data_dir = st.cfg.data_dir.clone().ok_or(ApiError::NotFound)?;
    let mut out = Vec::new();
    let files: Vec<(i64, String)> = sqlx::query_as(
        "SELECT n.id, r.rel_path FROM nodes n JOIN roots r ON r.id = n.root_id
         WHERE n.content_hash = $1 AND n.deleted_at IS NULL AND r.id = ANY($2) LIMIT 5",
    )
    .bind(hash.to_vec())
    .bind(&visible)
    .fetch_all(&st.db)
    .await?;
    for (id, rel) in files {
        out.push(data_dir.join(rel).join(db::rel_path(&st.db, id).await?));
    }
    let kept: Vec<String> = sqlx::query_scalar(
        "SELECT v.store_path FROM versions v JOIN nodes n ON n.id = v.node_id
         WHERE v.content_hash = $1 AND n.root_id = ANY($2) LIMIT 1",
    )
    .bind(hash.to_vec())
    .bind(&visible)
    .fetch_all(&st.db)
    .await?;
    let state = state_dir(st)?;
    out.extend(kept.into_iter().map(|p| state.join(p)));
    Ok(out)
}

/// Gets content ready for writing without receiving it again: uploaded ahead, or cloned from a
/// file or version that has it (checked by hashing, the database could be out of date).
async fn obtain(st: &AppState, user: i64, hash: &[u8; 32]) -> ApiResult<Option<Staged>> {
    let mut candidates = vec![uploaded_path(st, user, hash)?];
    candidates.extend(holders(st, user, hash).await?);
    for path in candidates {
        if let Some(staged) = content::stage_from(st, &path, hash).await? {
            return Ok(Some(staged));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
pub struct ContentQuery {
    pub size: Option<u64>,
}

/// Content ahead of an operation that needs it (raw body). Kept for a day.
pub async fn put_content(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(hash): Path<String>,
    Query(q): Query<ContentQuery>,
    body: Body,
) -> ApiResult<StatusCode> {
    let hash = hex32(&hash)?;
    let staged = content::stage(&st, body, q.size).await?;
    if *staged.hash() != hash {
        return Err(ApiError::bad("Der Inhalt passt nicht zum Hash."));
    }
    let to = uploaded_path(&st, me.id, &hash)?;
    tokio::task::spawn_blocking(move || staged.keep_as(&to))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct Available {
    pub available: bool,
}

/// Is the content available without uploading it (uploaded ahead, or present in a file or
/// version the person can read)?
pub async fn content_available(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(hash): Path<String>,
) -> ApiResult<Json<Available>> {
    let hash = hex32(&hash)?;
    let available = uploaded_path(&st, me.id, &hash)?.is_file()
        || !holders(&st, me.id, &hash).await?.is_empty();
    Ok(Json(Available { available }))
}

#[derive(Deserialize)]
pub struct OpRequest {
    /// Name of the device (part of the operation's identity, and of conflict copy names).
    pub device: String,
    pub op_id: u64,
    pub op: RemoteOp,
}

fn check_device(device: &str) -> ApiResult<()> {
    if device.trim().is_empty() || device.len() > 100 || device.chars().any(char::is_control) {
        return Err(ApiError::bad("Ungültiger Gerätename."));
    }
    Ok(())
}

async fn known(
    st: &AppState,
    user: i64,
    device: &str,
    op_id: u64,
) -> ApiResult<Option<RemoteResult>> {
    let r: Option<serde_json::Value> = sqlx::query_scalar(
        "SELECT result FROM sync_ops WHERE user_id = $1 AND device = $2 AND op_id = $3",
    )
    .bind(user)
    .bind(device)
    .bind(op_id as i64)
    .fetch_optional(&st.db)
    .await?;
    Ok(r.and_then(|v| serde_json::from_value(v).ok()))
}

/// Executes an operation of the sync engine (PLAN 5.3) once: a retry with the same id gets the
/// same answer. Preconditions that no longer hold are answered with the reason (`Rejected`);
/// nothing is overwritten. `Transient`: changed outside xlrx meanwhile, try again.
pub async fn op(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<OpRequest>,
) -> ApiResult<Json<RemoteResult>> {
    check_device(&req.device)?;
    let lock = st.sync_lock(me.id, &req.device);
    let _guard = lock.lock().await;
    if let Some(r) = known(&st, me.id, &req.device, req.op_id).await? {
        return Ok(Json(r));
    }
    let result = execute(&st, me.id, &req.device, &req.op).await?;
    if result != RemoteResult::Transient {
        sqlx::query(
            "INSERT INTO sync_ops (user_id, device, op_id, result) VALUES ($1, $2, $3, $4)
             ON CONFLICT DO NOTHING",
        )
        .bind(me.id)
        .bind(&req.device)
        .bind(req.op_id as i64)
        .bind(serde_json::to_value(&result).map_err(|e| ApiError::Internal(e.to_string()))?)
        .execute(&st.db)
        .await?;
    }
    Ok(Json(result))
}

/// The result of an operation executed before (for a client that lost the answer: it then needs
/// neither the source file nor a new upload).
pub async fn op_result(
    State(st): State<AppState>,
    me: CurrentUser,
    Path((device, op_id)): Path<(String, u64)>,
) -> ApiResult<Json<RemoteResult>> {
    known(&st, me.id, &device, op_id)
        .await?
        .map(Json)
        .ok_or(ApiError::NotFound)
}

fn created(n: &NodeRow) -> RemoteResult {
    RemoteResult::Created {
        node: NodeId(n.id as u64),
        rev: Rev(n.rev as u64),
        seq: Seq(n.seq as u64),
    }
}

fn missing_content() -> ApiError {
    ApiError::Conflict("Der Inhalt fehlt: zuerst hochladen (PUT /api/sync/content/{hash}).".into())
}

async fn execute(st: &AppState, user: i64, device: &str, op: &RemoteOp) -> ApiResult<RemoteResult> {
    // "Not found" means: the node (or the folder) is gone, or not visible to this person.
    let gone = |r: Reject| {
        move |e: ApiError| match e {
            ApiError::NotFound => ApiError::Rejected(r, String::new()),
            e => e,
        }
    };
    let id = |n: &NodeId| n.0 as i64;
    let res: ApiResult<RemoteResult> = match op {
        RemoteOp::CreateDir { parent, name, .. } => ops::mkdir(st, user, id(parent), name.as_str())
            .await
            .map_err(gone(Reject::ParentGone))
            .map(|n| created(&n)),
        RemoteOp::CreateFile {
            parent,
            name,
            content: c,
            ..
        } => {
            let Some(staged) = obtain(st, user, &c.hash.0).await? else {
                return Err(missing_content());
            };
            let target = Target::New {
                parent_id: id(parent),
                name: name.as_str().to_owned(),
                keep_both: false,
            };
            content::write(st, user, target, staged, None)
                .await
                .map_err(gone(Reject::ParentGone))
                .map(|(n, _)| created(&n))
        }
        RemoteOp::Upload {
            node,
            base_rev,
            content: c,
            ..
        } => {
            let Some(staged) = obtain(st, user, &c.hash.0).await? else {
                return Err(missing_content());
            };
            let target = Target::ReplaceOrConflict {
                node_id: id(node),
                base_rev: base_rev.0 as i64,
                device: device.to_owned(),
            };
            content::write(st, user, target, staged, None)
                .await
                .map_err(gone(Reject::NodeGone))
                .map(|(n, w)| match w {
                    Written::ConflictCopy => RemoteResult::Conflict {
                        node: NodeId(n.id as u64),
                        rev: Rev(n.rev as u64),
                        seq: Seq(n.seq as u64),
                    },
                    _ => RemoteResult::Updated {
                        rev: Rev(n.rev as u64),
                        seq: Seq(n.seq as u64),
                    },
                })
        }
        RemoteOp::Move {
            node,
            from_parent,
            from_name,
            parent,
            name,
        } => {
            let change = ops::Change {
                name: Some(name.as_str().to_owned()),
                parent_id: Some(id(parent)),
                if_seq: None,
                from: Some((id(from_parent), from_name.as_str().to_owned())),
            };
            ops::update(st, user, id(node), change)
                .await
                .map_err(gone(Reject::NodeGone))
                .map(|n| RemoteResult::Moved {
                    seq: Seq(n.seq as u64),
                })
        }
        RemoteOp::DeleteFile {
            node,
            base_rev,
            parent,
            name,
        } => {
            delete(
                st,
                user,
                id(node),
                false,
                ops::DeleteIf {
                    at: Some((id(parent), name.as_str().to_owned())),
                    rev: Some(base_rev.0 as i64),
                    empty: false,
                },
            )
            .await
        }
        RemoteOp::DeleteDir { node, parent, name } => {
            delete(
                st,
                user,
                id(node),
                true,
                ops::DeleteIf {
                    at: Some((id(parent), name.as_str().to_owned())),
                    rev: None,
                    empty: true,
                },
            )
            .await
        }
    };
    match res {
        Ok(r) => Ok(r),
        Err(ApiError::Rejected(r, _)) => Ok(RemoteResult::Rejected(r)),
        // Changed outside xlrx meanwhile (the root was scanned again): a retry decides anew.
        Err(ApiError::Conflict(_)) => Ok(RemoteResult::Transient),
        Err(e) => Err(e),
    }
}

async fn delete(
    st: &AppState,
    user: i64,
    node: i64,
    dir: bool,
    pre: ops::DeleteIf,
) -> ApiResult<RemoteResult> {
    let n = db::node_by_id(&st.db, node)
        .await?
        .filter(|n| n.deleted_at.is_none() && n.is_dir() == dir && n.parent_id.is_some())
        .ok_or_else(|| ApiError::Rejected(Reject::NodeGone, String::new()))?;
    ops::trash_if(st, user, n.id, None, &pre)
        .await
        .map_err(|e| match e {
            ApiError::NotFound => ApiError::Rejected(Reject::NodeGone, String::new()),
            e => e,
        })?;
    let after = db::node_by_id(&st.db, n.id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(RemoteResult::Deleted {
        seq: Seq(after.seq as u64),
    })
}

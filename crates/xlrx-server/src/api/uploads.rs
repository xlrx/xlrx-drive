//! Uploads in parts (PLAN 5.3): large files arrive in parts of at most 8 MiB, at any offset and in
//! any order. Each part is checked (optional SHA-256) and written to disk durably before it is
//! confirmed; after an interruption the client asks which ranges arrived and sends the rest. Only
//! a complete upload whose content checks out becomes a file – or, for sync clients, content
//! ready for an operation.
//!
//! Content-defined chunks with dedup against existing content (delta uploads) follow with the Mac
//! client (M5); they fit in as "ranges the server already has".

use std::os::unix::fs::FileExt as _;
use std::path::{Path as FsPath, PathBuf};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::files::{NodeInfo, editable, mtime};
use super::sync::{hex, hex32, uploaded_path};
use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::content::{self, Target};
use crate::files::{ops, store};
use crate::state::AppState;

/// Largest part per request (below the body limits of common proxies).
pub const PART_MAX: usize = 8 * 1024 * 1024;
/// An upload nobody touched for this long is removed.
pub const IDLE: Duration = Duration::hours(24);
/// Open uploads per person.
const MAX_OPEN: i64 = 200;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UploadTarget {
    /// A new file in a folder (like `POST /nodes/{id}/files`).
    New {
        parent_id: i64,
        name: String,
        #[serde(default)]
        keep_both: bool,
    },
    /// New content for a file, based on the revision the client knows.
    Replace { node_id: i64, base_rev: i64 },
    /// Content for a later sync operation (like `PUT /sync/content/{hash}`).
    Content,
}

#[derive(Deserialize)]
pub struct CreateReq {
    pub size: u64,
    pub target: UploadTarget,
    pub mtime_ms: Option<i64>,
}

#[derive(Serialize)]
pub struct UploadInfo {
    pub id: Uuid,
    pub size: i64,
    /// Ranges `[start, end)` that arrived, in order.
    pub received: Vec<[i64; 2]>,
    pub state: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct Row {
    size: i64,
    target: sqlx::types::Json<UploadTarget>,
    mtime_ms: Option<i64>,
    state: String,
    result: Option<Value>,
    ranges: sqlx::types::Json<Vec<[i64; 2]>>,
    complete: bool,
    updated_at: OffsetDateTime,
}

macro_rules! select_upload {
    () => {
        "SELECT size, target, mtime_ms, state, result, updated_at,
                (SELECT coalesce(jsonb_agg(jsonb_build_array(lower(r), upper(r)) ORDER BY lower(r)), '[]')
                   FROM unnest(received) r) AS ranges,
                received = int8multirange(int8range(0, size)) AS complete
           FROM uploads WHERE id = $1 AND user_id = $2"
    };
}
const SELECT: &str = select_upload!();
const SELECT_FOR_UPDATE: &str = concat!(select_upload!(), " FOR UPDATE");

fn not_found() -> ApiError {
    ApiError::NotFound
}

fn info(id: Uuid, row: &Row) -> UploadInfo {
    UploadInfo {
        id,
        size: row.size,
        received: row.ranges.0.clone(),
        state: row.state.clone(),
        expires_at: row.updated_at + IDLE,
    }
}

fn uploads_dir(st: &AppState) -> ApiResult<PathBuf> {
    Ok(st
        .cfg
        .state_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))?
        .join(store::UPLOADS))
}

fn part_path(st: &AppState, id: Uuid) -> ApiResult<PathBuf> {
    Ok(uploads_dir(st)?.join(id.simple().to_string()))
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| {
            if e.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error()) {
                ApiError::Conflict("Kein Platz mehr auf dem NAS.".into())
            } else {
                ApiError::Internal(format!("Upload: {e}"))
            }
        })
}

/// Checks the target as far as possible before anything arrives (the same rules apply again when
/// the upload is put in place).
async fn check_target(st: &AppState, me: &CurrentUser, target: &UploadTarget) -> ApiResult<()> {
    match target {
        UploadTarget::New {
            parent_id,
            name,
            keep_both,
        } => {
            let name = ops::valid_name(name)?;
            let (folder, _) = editable(st, me, *parent_id).await?;
            if !folder.is_dir() {
                return Err(ApiError::bad("Kein Ordner."));
            }
            if !keep_both && ops::name_in_use(st, *parent_id, &name).await? {
                return Err(ApiError::Conflict(format!("„{name}“ gibt es hier schon.")));
            }
        }
        UploadTarget::Replace { node_id, base_rev } => {
            let (node, _) = editable(st, me, *node_id).await?;
            if node.is_dir() {
                return Err(ApiError::bad("Ordner haben keinen Inhalt."));
            }
            if node.rev != *base_rev {
                return Err(ApiError::Conflict(format!(
                    "„{}“ wurde inzwischen geändert. Bitte neu laden; beide Fassungen bleiben so erhalten.",
                    node.name
                )));
            }
        }
        UploadTarget::Content => {}
    }
    Ok(())
}

/// Starts an upload.
pub async fn create(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<CreateReq>,
) -> ApiResult<(StatusCode, Json<UploadInfo>)> {
    let size = i64::try_from(req.size).map_err(|_| ApiError::bad("Zu groß."))?;
    check_target(&st, &me, &req.target).await?;
    let open: i64 = sqlx::query_scalar("SELECT count(*) FROM uploads WHERE user_id = $1")
        .bind(me.id)
        .fetch_one(&st.db)
        .await?;
    if open >= MAX_OPEN {
        return Err(ApiError::bad(
            "Zu viele offene Uploads. Bitte zuerst andere abschließen.",
        ));
    }
    let dir = uploads_dir(&st)?;
    let state_dir = dir
        .parent()
        .and_then(FsPath::parent)
        .map(FsPath::to_path_buf);
    let free = blocking(move || {
        if let Some(d) = &state_dir {
            store::init(d)?;
        }
        let s = rustix::fs::statvfs(&dir)?;
        Ok(s.f_bavail.saturating_mul(s.f_frsize))
    })
    .await?;
    if req.size > free {
        return Err(ApiError::Conflict(
            "Nicht genug freier Platz auf dem NAS für diese Datei.".into(),
        ));
    }
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO uploads (user_id, size, target, mtime_ms) VALUES ($1, $2, $3, $4) RETURNING id",
    )
    .bind(me.id)
    .bind(size)
    .bind(sqlx::types::Json(&req.target))
    .bind(req.mtime_ms)
    .fetch_one(&st.db)
    .await?;
    let path = part_path(&st, id)?;
    blocking(move || std::fs::File::create_new(path).map(drop)).await?;
    let row: Row = sqlx::query_as(SELECT)
        .bind(id)
        .bind(me.id)
        .fetch_one(&st.db)
        .await?;
    Ok((StatusCode::CREATED, Json(info(id, &row))))
}

pub async fn status(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<UploadInfo>> {
    let row: Row = sqlx::query_as(SELECT)
        .bind(id)
        .bind(me.id)
        .fetch_optional(&st.db)
        .await?
        .ok_or_else(not_found)?;
    Ok(Json(info(id, &row)))
}

#[derive(Deserialize)]
pub struct PartQuery {
    pub offset: u64,
}

/// One part at an offset. Confirmed only once it is on disk; a part that arrived before (a retry
/// whose answer got lost) is confirmed again without writing.
pub async fn put_part(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<Uuid>,
    Query(q): Query<PartQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Json<UploadInfo>> {
    let data = axum::body::to_bytes(body, PART_MAX)
        .await
        .map_err(|_| ApiError::bad("Teil zu groß oder abgebrochen (höchstens 8 MiB pro Teil)."))?;
    if data.is_empty() {
        return Err(ApiError::bad("Leerer Teil."));
    }
    if let Some(expected) = headers.get("x-content-sha256") {
        let expected = expected
            .to_str()
            .ok()
            .and_then(|v| hex32(v.trim()).ok())
            .ok_or_else(|| ApiError::bad("Ungültige Prüfsumme."))?;
        if Sha256::digest(&data).as_slice() != expected {
            return Err(ApiError::bad(
                "Der Teil kam beschädigt an (Prüfsumme stimmt nicht). Bitte erneut senden.",
            ));
        }
    }
    let start = i64::try_from(q.offset).map_err(|_| ApiError::bad("Ungültiger Offset."))?;
    let end = start
        .checked_add(data.len() as i64)
        .ok_or_else(|| ApiError::bad("Ungültiger Offset."))?;

    // The row lock keeps parts of one upload from deciding about the same range at the same time.
    let mut tx = st.db.begin().await?;
    let found: Option<(i64, String, bool, bool)> = sqlx::query_as(
        "SELECT size, state, received @> int8range($3, $4), received && int8range($3, $4)
           FROM uploads WHERE id = $1 AND user_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(me.id)
    .bind(start)
    .bind(end)
    .fetch_optional(&mut *tx)
    .await?;
    let (size, state, have, overlaps) = found.ok_or_else(not_found)?;
    if state != "open" {
        return Err(ApiError::Conflict(
            "Dieser Upload ist schon abgeschlossen.".into(),
        ));
    }
    if end > size {
        return Err(ApiError::bad("Der Teil reicht über das Dateiende hinaus."));
    }
    if !have {
        if overlaps {
            return Err(ApiError::Conflict(
                "Der Teil überschneidet sich mit schon empfangenen Daten.".into(),
            ));
        }
        let path = part_path(&st, id)?;
        blocking(move || {
            let f = std::fs::OpenOptions::new().write(true).open(path)?;
            f.write_all_at(&data, q.offset)?;
            f.sync_data()
        })
        .await?;
        sqlx::query(
            "UPDATE uploads SET received = received + int8multirange(int8range($2, $3)),
                    updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(start)
        .bind(end)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    status(State(st), me, Path(id)).await
}

#[derive(Deserialize, Default)]
pub struct CommitReq {
    /// Content hash (xlrx-content-v1) the client computed; checked when given.
    pub hash: Option<String>,
    /// New file: keep both if the name was taken meanwhile (instead of refusing).
    pub keep_both: Option<bool>,
}

fn unclear() -> ApiError {
    ApiError::Conflict(
        "Ob dieser Upload gespeichert wurde, ist unklar. Bitte im Ordner nachsehen und die Datei \
         gegebenenfalls erneut hochladen."
            .into(),
    )
}

/// Puts a complete upload in place. Repeating it returns the same result.
pub async fn commit(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<Uuid>,
    body: Option<Json<CommitReq>>,
) -> ApiResult<Json<Value>> {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let expected = req.hash.as_deref().map(hex32).transpose()?;
    let mut tx = st.db.begin().await?;
    let row: Row = sqlx::query_as(SELECT_FOR_UPDATE)
        .bind(id)
        .bind(me.id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(not_found)?;
    match row.state.as_str() {
        "committed" => return Ok(Json(row.result.unwrap_or(Value::Null))),
        "committing" => return Err(unclear()),
        _ => {}
    }
    if !row.complete {
        return Err(ApiError::Conflict(
            "Der Upload ist noch nicht vollständig.".into(),
        ));
    }
    let mut target = row.target.0.clone();
    if let (UploadTarget::New { keep_both, .. }, Some(true)) = (&mut target, req.keep_both) {
        *keep_both = true;
    }
    // Refused for a reason that might change (name taken, file changed): the parts stay, the
    // client can decide and commit again.
    check_target(&st, &me, &target).await?;
    sqlx::query("UPDATE uploads SET state = 'committing', updated_at = now() WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let result = put_in_place(&st, &me, id, target, row.mtime_ms, expected).await;
    match result {
        Ok(value) => {
            sqlx::query(
                "UPDATE uploads SET state = 'committed', result = $2, received = '{}', updated_at = now()
                  WHERE id = $1",
            )
            .bind(id)
            .bind(&value)
            .execute(&st.db)
            .await?;
            Ok(Json(value))
        }
        Err(e) => {
            // The parts are gone (a wrong hash, or consumed by the attempt): start over.
            sqlx::query("DELETE FROM uploads WHERE id = $1")
                .bind(id)
                .execute(&st.db)
                .await?;
            Err(e)
        }
    }
}

async fn put_in_place(
    st: &AppState,
    me: &CurrentUser,
    id: Uuid,
    target: UploadTarget,
    mtime_ms: Option<i64>,
    expected: Option<[u8; 32]>,
) -> ApiResult<Value> {
    let staged = content::adopt(part_path(st, id)?).await?;
    if expected.is_some_and(|h| h != *staged.hash()) {
        return Err(ApiError::bad(
            "Der Inhalt passt nicht zum angegebenen Hash. Bitte erneut hochladen.",
        ));
    }
    let target = match target {
        UploadTarget::Content => {
            let hash = *staged.hash();
            let to = uploaded_path(st, me.id, &hash)?;
            blocking(move || staged.keep_as(&to)).await?;
            return Ok(json!({ "hash": hex(&hash) }));
        }
        UploadTarget::New {
            parent_id,
            name,
            keep_both,
        } => Target::New {
            parent_id,
            name,
            keep_both,
        },
        UploadTarget::Replace { node_id, base_rev } => Target::Replace { node_id, base_rev },
    };
    let (node, _) = content::write(st, me.id, target, staged, mtime(mtime_ms)).await?;
    serde_json::to_value(NodeInfo::from(&node)).map_err(|e| ApiError::Internal(e.to_string()))
}

/// Gives up an upload; its parts are removed.
pub async fn abort(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    let state: Option<String> = sqlx::query_scalar(
        "DELETE FROM uploads WHERE id = $1 AND user_id = $2 AND state <> 'committing' RETURNING state",
    )
    .bind(id)
    .bind(me.id)
    .fetch_optional(&st.db)
    .await?;
    if state.is_none() {
        return Err(ApiError::NotFound);
    }
    let path = part_path(&st, id)?;
    blocking(move || match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// After a restart: an upload whose putting in place was interrupted goes back to "open" if its
/// file is still there – recovery (run before) removes the file of any write it took over, so
/// nothing was written from it. Without the file the outcome stays "unclear".
pub async fn recover(st: &AppState) -> ApiResult<()> {
    let committing: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM uploads WHERE state = 'committing'")
            .fetch_all(&st.db)
            .await?;
    for id in committing {
        if part_path(st, id)?.is_file() {
            sqlx::query("UPDATE uploads SET state = 'open' WHERE id = $1 AND state = 'committing'")
                .bind(id)
                .execute(&st.db)
                .await?;
        }
    }
    Ok(())
}

/// Removes uploads nobody touched for a day, and files of uploads that no longer exist.
pub async fn housekeeping(st: &AppState) -> ApiResult<()> {
    let expired: Vec<Uuid> = sqlx::query_scalar(
        "DELETE FROM uploads WHERE updated_at < now() - make_interval(secs => $1) RETURNING id",
    )
    .bind(IDLE.whole_seconds() as f64)
    .fetch_all(&st.db)
    .await?;
    let known: std::collections::HashSet<Uuid> =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM uploads")
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    let dir = uploads_dir(st)?;
    blocking(move || {
        for id in expired {
            let _ = std::fs::remove_file(dir.join(id.simple().to_string()));
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(());
        };
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for e in entries.flatten() {
            let orphan = e
                .file_name()
                .to_str()
                .and_then(|n| Uuid::try_parse(n).ok())
                .is_some_and(|id| !known.contains(&id));
            // Recent files may belong to an upload being created right now.
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < hour_ago);
            if orphan && old {
                let _ = std::fs::remove_file(e.path());
            }
        }
        Ok(())
    })
    .await
}

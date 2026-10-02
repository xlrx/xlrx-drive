//! Browsing and downloading: roots, nodes, children, content.

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{
    CONTENT_DISPOSITION, CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, X_FRAME_OPTIONS,
};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::content::{self as file_content, Target, VersionInfo};
use crate::files::db::{self, NODE_COLS, NodeRow, RootRow};
use crate::files::ops::{self, TrashItem};
use crate::files::roots;
use crate::state::AppState;

#[derive(Serialize)]
pub struct RootInfo {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub node_id: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scanned_at: Option<OffsetDateTime>,
}

/// The roots this person can see (the own "My Drive" is created on first access).
pub async fn list_roots(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<RootInfo>>> {
    let Some(home) = roots::ensure_home(&st, me.id, &me.username).await? else {
        return Ok(Json(vec![]));
    };
    if home.scanned_at.is_none() {
        // First access: import what is already there (e.g. the Synology Drive folder).
        let st2 = st.clone();
        let r = home.clone();
        tokio::spawn(async move {
            if let Err(e) = roots::scan(&st2, &r).await {
                tracing::warn!(root = r.id, error = ?e, "Erster Abgleich fehlgeschlagen");
            }
        });
    }
    let node = db::root_node(&st.db, home.id)
        .await?
        .ok_or_else(|| ApiError::Internal("Ablage ohne Wurzelknoten".into()))?;
    Ok(Json(vec![RootInfo {
        id: home.id,
        kind: home.kind,
        name: home.name,
        node_id: node.id,
        scanned_at: home.scanned_at,
    }]))
}

pub async fn scan_root(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<crate::files::scan::ScanReport>> {
    let root = db::root_by_id(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !roots::can_read(&root, me.id) {
        return Err(ApiError::NotFound);
    }
    Ok(Json(roots::scan(&st, &root).await?))
}

#[derive(Serialize)]
pub struct NodeInfo {
    pub id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub size: Option<i64>,
    pub rev: i64,
    /// Last change of any kind (for `if_seq` preconditions).
    pub seq: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub mtime: Option<OffsetDateTime>,
    pub mime: Option<String>,
}

impl From<&NodeRow> for NodeInfo {
    fn from(n: &NodeRow) -> Self {
        Self {
            id: n.id,
            parent_id: n.parent_id,
            name: n.name.clone(),
            kind: n.kind.clone(),
            size: n.size,
            rev: n.rev,
            seq: n.seq,
            mtime: n.mtime,
            mime: (!n.is_dir()).then(|| {
                mime_guess::from_path(&n.name)
                    .first_or_octet_stream()
                    .to_string()
            }),
        }
    }
}

/// A live node the person may see (otherwise "not found", without revealing existence).
pub async fn visible(st: &AppState, me: &CurrentUser, id: i64) -> ApiResult<(NodeRow, RootRow)> {
    let node = db::node_by_id(&st.db, id)
        .await?
        .filter(|n| n.deleted_at.is_none())
        .ok_or(ApiError::NotFound)?;
    let root = db::root_by_id(&st.db, node.root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !roots::can_read(&root, me.id) {
        return Err(ApiError::NotFound);
    }
    Ok((node, root))
}

#[derive(Serialize)]
pub struct NodeDetail {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// From the root directory down to the node.
    pub path: Vec<Crumb>,
    pub root_id: i64,
}

#[derive(Serialize)]
pub struct Crumb {
    pub id: i64,
    pub name: String,
}

pub async fn get_node(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<NodeDetail>> {
    let (node, root) = visible(&st, &me, id).await?;
    let path = db::ancestors(&st.db, node.id)
        .await?
        .iter()
        .enumerate()
        .map(|(i, n)| Crumb {
            id: n.id,
            // The root directory is shown with the root's name ("Meine Ablage").
            name: if i == 0 {
                root.name.clone()
            } else {
                n.name.clone()
            },
        })
        .collect();
    Ok(Json(NodeDetail {
        node: NodeInfo::from(&node),
        path,
        root_id: root.id,
    }))
}

pub async fn children(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<NodeInfo>>> {
    let (node, _) = visible(&st, &me, id).await?;
    if !node.is_dir() {
        return Err(ApiError::bad("Kein Ordner."));
    }
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE parent_id = $1 AND deleted_at IS NULL
         ORDER BY kind, name_folded, name"
    )))
    .bind(node.id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows.iter().map(NodeInfo::from).collect()))
}

#[derive(Deserialize)]
pub struct ContentQuery {
    /// Show in the browser instead of downloading (only for harmless types).
    #[serde(default)]
    pub inline: bool,
}

/// Types the browser may display directly (the web app mirrors this in `opensInBrowser`). Everything else (HTML, SVG, scripts …) is only
/// offered as a download.
fn inline_allowed(mime: &str) -> bool {
    (mime.starts_with("image/") && mime != "image/svg+xml")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || mime == "application/pdf"
        || mime == "text/plain"
}

fn disposition(kind: &str, name: &str) -> HeaderValue {
    let ascii: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let encoded: String = url::form_urlencoded::byte_serialize(name.as_bytes())
        .collect::<String>()
        .replace('+', "%20");
    HeaderValue::from_str(&format!(
        "{kind}; filename=\"{ascii}\"; filename*=UTF-8''{encoded}"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// File content with range requests (resumable downloads, video seeking).
pub async fn content(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ContentQuery>,
    req: Request,
) -> ApiResult<Response> {
    let (node, root) = visible(&st, &me, id).await?;
    if node.is_dir() {
        return Err(ApiError::bad("Ordner können nicht heruntergeladen werden."));
    }
    let data_dir = st.cfg.data_dir.as_deref().ok_or(ApiError::NotFound)?;
    let path = roots::dir(data_dir, &root).join(db::rel_path(&st.db, node.id).await?);
    serve(
        req,
        &path,
        &node.name,
        q.inline,
        node.content_hash.as_deref(),
    )
    .await
}

/// Serves a file of a person with the safety headers for user content.
async fn serve(
    req: Request,
    path: &std::path::Path,
    name: &str,
    inline: bool,
    hash: Option<&[u8]>,
) -> ApiResult<Response> {
    let mime = mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string();
    let inline = inline && inline_allowed(&mime);
    let mut res = ServeFile::new(path)
        .oneshot(req)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map(axum::body::Body::new);
    if res.status() == StatusCode::NOT_FOUND {
        return Err(ApiError::NotFound);
    }
    let h = res.headers_mut();
    h.insert(
        CONTENT_DISPOSITION,
        disposition(if inline { "inline" } else { "attachment" }, name),
    );
    if !inline {
        h.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
    }
    // User content never runs scripts in our origin. Previews may be framed by the web app;
    // PDF needs the browser's viewer and therefore no sandbox.
    let csp = match (inline, mime == "application/pdf") {
        (false, _) => "sandbox; default-src 'none'; frame-ancestors 'none'",
        (true, true) => {
            "default-src 'none'; object-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'self'"
        }
        (true, false) => {
            "sandbox; default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'; \
             frame-ancestors 'self'"
        }
    };
    h.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(csp));
    if inline {
        h.insert(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
    }
    if let Some(hash) = hash {
        let tag: String = hash.iter().take(16).map(|b| format!("{b:02x}")).collect();
        if let Ok(v) = HeaderValue::from_str(&format!("\"{tag}\"")) {
            h.insert(ETAG, v);
        }
    }
    Ok(res.into_response())
}

#[derive(Deserialize)]
pub struct NewFolder {
    pub name: String,
}

pub async fn create_folder(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(parent): Path<i64>,
    Json(b): Json<NewFolder>,
) -> ApiResult<(StatusCode, Json<NodeInfo>)> {
    let n = ops::mkdir(&st, me.id, parent, &b.name).await?;
    Ok((StatusCode::CREATED, Json(NodeInfo::from(&n))))
}

/// Rename and/or move.
pub async fn update_node(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Json(ch): Json<ops::Change>,
) -> ApiResult<Json<NodeInfo>> {
    let n = ops::update(&st, me.id, id, ch).await?;
    Ok(Json(NodeInfo::from(&n)))
}

#[derive(Deserialize)]
pub struct IfSeq {
    pub if_seq: Option<i64>,
}

/// Into the trash.
pub async fn delete_node(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<IfSeq>,
) -> ApiResult<StatusCode> {
    ops::trash(&st, me.id, id, q.if_seq).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn trash_list(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(root): Path<i64>,
) -> ApiResult<Json<Vec<TrashItem>>> {
    Ok(Json(ops::trash_list(&st, me.id, root).await?))
}

pub async fn restore(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<NodeInfo>> {
    let n = ops::restore(&st, me.id, id).await?;
    Ok(Json(NodeInfo::from(&n)))
}

/// Delete from the trash for good.
pub async fn purge(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    ops::purge(&st, me.id, id).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct UploadQuery {
    pub name: String,
    /// If the name is taken: "Name (1).ext" instead of refusing.
    #[serde(default)]
    pub keep_both: bool,
    /// Modification time of the original (ms since 1970), as browsers report it.
    pub mtime_ms: Option<i64>,
    /// Announced length; a shorter or longer body is refused.
    pub size: Option<u64>,
}

fn mtime(ms: Option<i64>) -> Option<OffsetDateTime> {
    ms.and_then(|ms| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000).ok())
}

/// A new file in a folder (raw request body).
pub async fn upload(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(parent): Path<i64>,
    Query(q): Query<UploadQuery>,
    body: axum::body::Body,
) -> ApiResult<(StatusCode, Json<NodeInfo>)> {
    // Fail fast before receiving gigabytes that would be refused anyway.
    let name = ops::valid_name(&q.name)?;
    let (folder, root) = visible(&st, &me, parent).await?;
    if !roots::can_write(&root, me.id) {
        return Err(ApiError::forbidden("Keine Schreibrechte in diesem Ordner."));
    }
    if !folder.is_dir() {
        return Err(ApiError::bad("Kein Ordner."));
    }
    if !q.keep_both && ops::name_in_use(&st, parent, &name).await? {
        return Err(ApiError::Conflict(format!("„{name}“ gibt es hier schon.")));
    }
    let staged = file_content::stage(&st, body, q.size).await?;
    let (node, _) = file_content::write(
        &st,
        me.id,
        Target::New {
            parent_id: parent,
            name: q.name,
            keep_both: q.keep_both,
        },
        staged,
        mtime(q.mtime_ms),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(NodeInfo::from(&node))))
}

#[derive(Deserialize)]
pub struct ReplaceQuery {
    /// Revision the new content is based on; anything else is a conflict.
    pub base_rev: i64,
    pub mtime_ms: Option<i64>,
    pub size: Option<u64>,
}

/// New content for an existing file.
pub async fn replace_content(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ReplaceQuery>,
    body: axum::body::Body,
) -> ApiResult<Json<NodeInfo>> {
    let (node, root) = visible(&st, &me, id).await?;
    if !roots::can_write(&root, me.id) {
        return Err(ApiError::forbidden("Keine Schreibrechte für diese Datei."));
    }
    if node.is_dir() {
        return Err(ApiError::bad("Ordner haben keinen Inhalt."));
    }
    if node.rev != q.base_rev {
        return Err(ApiError::Conflict(format!(
            "„{}“ wurde inzwischen geändert. Bitte neu laden; beide Fassungen bleiben so erhalten.",
            node.name
        )));
    }
    let staged = file_content::stage(&st, body, q.size).await?;
    let (node, _) = file_content::write(
        &st,
        me.id,
        Target::Replace {
            node_id: id,
            base_rev: q.base_rev,
        },
        staged,
        mtime(q.mtime_ms),
    )
    .await?;
    Ok(Json(NodeInfo::from(&node)))
}

pub async fn versions(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<VersionInfo>>> {
    Ok(Json(file_content::versions(&st, me.id, id).await?))
}

pub async fn version_content(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ContentQuery>,
    req: Request,
) -> ApiResult<Response> {
    let (path, node, v) = file_content::version_file(&st, me.id, id).await?;
    serve(req, &path, &node.name, q.inline, Some(&v.content_hash)).await
}

pub async fn restore_version(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<NodeInfo>> {
    let node = file_content::restore_version(&st, me.id, id).await?;
    Ok(Json(NodeInfo::from(&node)))
}

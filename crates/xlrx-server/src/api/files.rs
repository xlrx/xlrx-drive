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

use crate::auth::session::{ClientInfo, CurrentUser};
use crate::error::{ApiError, ApiResult};
use crate::files::access::{self, Role, Scope};
use crate::files::content::{self as file_content, Target, VersionInfo};
use crate::files::data_class::{self, Class};
use crate::files::db::{self, NODE_COLS, NodeRow, RootRow};
use crate::files::mirror;
use crate::files::ops::{self, TrashItem};
use crate::files::{roots, thumbs};
use crate::state::AppState;

#[derive(Serialize)]
pub struct RootInfo {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub node_id: i64,
    /// The person's role over the whole root.
    pub role: Role,
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
            if st2.cfg.watch
                && let Err(e) = crate::files::watch::start(&st2, &r).await
            {
                tracing::warn!(root = r.id, error = ?e, "Überwachung nicht gestartet");
            }
            if let Err(e) = roots::scan(&st2, &r).await {
                tracing::warn!(root = r.id, error = ?e, "Erster Abgleich fehlgeschlagen");
            }
        });
    }
    // The own home first, then the shared roots the person is a member of.
    let roles = access::root_roles(&st.db, me.id).await?;
    let mut out = Vec::new();
    for r in roots::readable(&st, me.id).await? {
        let Some(node) = db::root_node(&st.db, r.id).await? else {
            continue;
        };
        out.push(RootInfo {
            id: r.id,
            role: roles.get(&r.id).copied().unwrap_or(Role::Viewer),
            kind: r.kind,
            name: r.name,
            node_id: node.id,
            scanned_at: r.scanned_at,
        });
    }
    out.sort_by_key(|r| (r.kind != "home", r.name.to_lowercase()));
    Ok(Json(out))
}

pub async fn scan_root(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<crate::files::scan::ScanReport>> {
    let root = db::root_by_id(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if roots::role(&st, &root, me.id).await?.is_none() {
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
    let a = access::require(&st.db, me.id, id, Role::Viewer).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    Ok((a.node, a.root))
}

/// A live node the person may change (a folder to add to, a file to write).
pub async fn editable(st: &AppState, me: &CurrentUser, id: i64) -> ApiResult<(NodeRow, RootRow)> {
    let a = access::require(&st.db, me.id, id, Role::Editor).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    Ok((a.node, a.root))
}

#[derive(Serialize)]
pub struct NodeDetail {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// From the highest folder the person can see down to the node: the root directory, or for
    /// items shared with them, the shared folder (nothing above it is revealed).
    pub path: Vec<Crumb>,
    pub root_id: i64,
    /// The person's role here.
    pub role: Role,
    /// Seen through a share, not as owner or member of the whole root.
    pub shared: bool,
    /// May its content leave the home network (PLAN 7.4)?
    pub data_class: DataClassInfo,
    /// Starred by the person ("Markiert").
    pub starred: bool,
}

#[derive(Serialize)]
pub struct DataClassInfo {
    pub class: Class,
    /// Set on this item itself.
    pub explicit: bool,
    /// The folder it is inherited from, if the person can see that folder.
    pub from: Option<Crumb>,
    /// No folder says anything: the server's default applies.
    pub default: bool,
    /// May the person change it (folders they manage)?
    pub can_change: bool,
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
    let a = access::require(&st.db, me.id, id, Role::Viewer).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    let scope = access::scope(&st.db, me.id).await?;
    let whole = a.root_role.is_some();
    let chain = access::chains(&st.db, &scope, &[a.node.id])
        .await?
        .remove(&a.node.id)
        .unwrap_or_default();
    let path: Vec<Crumb> = chain
        .into_iter()
        .enumerate()
        .map(|(i, (id, name))| Crumb {
            id,
            // The root directory is shown with the root's name ("Meine Ablage").
            name: if i == 0 && whole {
                a.root.name.clone()
            } else {
                name
            },
        })
        .collect();
    let eff = data_class::effective(&st.db, st.cfg.default_data_class, &[a.node.id])
        .await?
        .remove(&a.node.id)
        .unwrap_or(data_class::Effective {
            class: st.cfg.default_data_class,
            from: None,
        });
    let data_class = DataClassInfo {
        class: eff.class,
        explicit: eff.from == Some(a.node.id),
        // Only a folder the person can see is named; a setting further up stays anonymous.
        from: eff.from.and_then(|f| {
            path.iter().find(|c| c.id == f).map(|c| Crumb {
                id: c.id,
                name: c.name.clone(),
            })
        }),
        default: eff.from.is_none(),
        can_change: a.node.is_dir() && a.role >= Role::Manager,
    };
    Ok(Json(NodeDetail {
        node: NodeInfo::from(&a.node),
        path,
        root_id: a.root.id,
        role: a.role,
        shared: !whole,
        data_class,
        starred: !super::stars::starred(&st, me.id, &[a.node.id])
            .await?
            .is_empty(),
    }))
}

#[derive(Serialize)]
pub struct ChildInfo {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// A data class set on this folder itself (badge "NUR LOKAL").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_class: Option<Class>,
    /// Starred by the person.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub starred: bool,
}

pub async fn children(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<ChildInfo>>> {
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
    let ids: Vec<i64> = rows.iter().map(|n| n.id).collect();
    let classes = data_class::explicit(&st.db, &ids).await?;
    let stars = super::stars::starred(&st, me.id, &ids).await?;
    Ok(Json(
        rows.iter()
            .map(|n| ChildInfo {
                node: NodeInfo::from(n),
                data_class: classes.get(&n.id).copied(),
                starred: stars.contains(&n.id),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
pub struct ContentQuery {
    /// Show in the browser instead of downloading (only for harmless types).
    #[serde(default)]
    pub inline: bool,
}

/// Types the browser may display directly (the web app mirrors this in `opensInBrowser`). Everything else (HTML, SVG, scripts …) is only
/// offered as a download.
pub(crate) fn inline_allowed(mime: &str) -> bool {
    (mime.starts_with("image/") && mime != "image/svg+xml")
        || mime.starts_with("video/")
        || mime.starts_with("audio/")
        || mime == "application/pdf"
        || mime == "text/plain"
}

pub(crate) fn disposition(kind: &str, name: &str) -> HeaderValue {
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

/// Whether a request starts a download (not the continuation of one): those are counted.
pub(crate) fn starts_download(headers: &axum::http::HeaderMap) -> bool {
    match headers
        .get(axum::http::header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        None => true,
        Some(r) => r
            .trim()
            .strip_prefix("bytes=")
            .is_some_and(|r| r.trim_start().starts_with("0-")),
    }
}

/// File content with range requests (resumable downloads, video seeking).
pub async fn content(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Query(q): Query<ContentQuery>,
    req: Request,
) -> ApiResult<Response> {
    let (node, root) = visible(&st, &me, id).await?;
    if node.is_dir() {
        return Err(ApiError::bad("Ordner können nicht heruntergeladen werden."));
    }
    let inline = q.inline && inline_allowed(&mime_of(&node.name));
    // A download (not a preview, not the continuation of one) for the start page.
    if !inline && starts_download(req.headers()) {
        super::suggest::record(&st, me.id, node.id, "download", "web", None).await?;
        mirror::fetched(&st, &client.ip, &node).await?;
    }
    if let Some(url) = mirror::redirect(&st, &client.ip, &node, inline).await {
        return Ok(to_outside_cache(&url));
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

/// The type a file is served as (from its name).
pub(crate) fn mime_of(name: &str) -> String {
    mime_guess::from_path(name)
        .first_or_octet_stream()
        .to_string()
}

/// From outside the home network: the file from the outside cache (PLAN 15.2).
pub(crate) fn to_outside_cache(url: &str) -> Response {
    (
        StatusCode::TEMPORARY_REDIRECT,
        [
            (axum::http::header::LOCATION, url),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
    )
        .into_response()
}

/// Serves a file of a person with the safety headers for user content.
pub(crate) async fn serve(
    req: Request,
    path: &std::path::Path,
    name: &str,
    inline: bool,
    hash: Option<&[u8]>,
) -> ApiResult<Response> {
    let mime = mime_of(name);
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

#[derive(Serialize)]
pub struct RecentItem {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// The folder it is in, e.g. "Meine Ablage/Belege".
    pub folder: String,
}

#[derive(Deserialize)]
pub struct RecentQuery {
    pub limit: Option<i64>,
}

/// Files changed last in the roots the person can read (start page).
pub async fn recent(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<RecentQuery>,
) -> ApiResult<Json<Vec<RecentItem>>> {
    let roots = roots::readable(&st, me.id).await?;
    let ids: Vec<i64> = roots.iter().map(|r| r.id).collect();
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes
          WHERE root_id = ANY($1) AND kind = 'file' AND deleted_at IS NULL
          ORDER BY mtime DESC NULLS LAST, id DESC LIMIT $2"
    )))
    .bind(&ids)
    .bind(q.limit.unwrap_or(8).clamp(1, 50))
    .fetch_all(&st.db)
    .await?;
    let scope = access::scope(&st.db, me.id).await?;
    let folders = folder_names(&st, &scope, &rows).await?;
    let out = rows
        .iter()
        .map(|n| RecentItem {
            node: NodeInfo::from(n),
            folder: folders.get(&n.id).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(Json(out))
}

/// Label of the place above items shared with a person (instead of the owner's folders).
pub const SHARED_LABEL: &str = "Geteilt";

/// The folder each node is in as the person sees it, e.g. "Meine Ablage/Belege" (the root's name,
/// then the directories below its root directory), or "Geteilt/Urlaub" for items shared with
/// them: folders above what was shared are never named.
pub async fn folder_names(
    st: &AppState,
    scope: &Scope,
    nodes: &[NodeRow],
) -> ApiResult<std::collections::HashMap<i64, String>> {
    let parents: Vec<i64> = nodes.iter().filter_map(|n| n.parent_id).collect();
    let chains = access::chains(&st.db, scope, &parents).await?;
    let root_ids: Vec<i64> = nodes.iter().map(|n| n.root_id).collect();
    let names: std::collections::HashMap<i64, String> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, name FROM roots WHERE id = ANY($1)")
            .bind(&root_ids)
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    Ok(nodes
        .iter()
        .map(|n| {
            let chain = n.parent_id.and_then(|p| chains.get(&p));
            let folder = if scope.roots.contains(&n.root_id) {
                let mut parts = vec![names.get(&n.root_id).cloned().unwrap_or_default()];
                parts.extend(
                    chain
                        .into_iter()
                        .flatten()
                        .skip(1)
                        .map(|(_, name)| name.clone()),
                );
                parts.join("/")
            } else {
                let mut parts = vec![SHARED_LABEL.to_string()];
                parts.extend(chain.into_iter().flatten().map(|(_, name)| name.clone()));
                parts.join("/")
            };
            (n.id, folder)
        })
        .collect())
}

#[derive(Deserialize)]
pub struct ThumbQuery {
    /// Longest side in pixels (one of [`thumbs::SIZES`]).
    pub s: u32,
    /// The revision the caller knows: if it is current, the browser may keep the answer.
    pub v: Option<i64>,
}

/// Thumbnail of an image; 404 for anything that has none.
pub async fn thumbnail(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Query(q): Query<ThumbQuery>,
) -> ApiResult<Response> {
    let (node, root) = visible(&st, &me, id).await?;
    thumbnail_of(&st, &node, &root, &q).await
}

/// The thumbnail of a node already checked to be visible.
pub(crate) async fn thumbnail_of(
    st: &AppState,
    node: &NodeRow,
    root: &RootRow,
    q: &ThumbQuery,
) -> ApiResult<Response> {
    if !thumbs::SIZES.contains(&q.s) {
        return Err(ApiError::bad("Ungültige Größe."));
    }
    let mime = mime_guess::from_path(&node.name).first_or_octet_stream();
    if node.is_dir() || !thumbs::supported(mime.essence_str()) {
        return Err(ApiError::NotFound);
    }
    let data_dir = st.cfg.data_dir.clone().ok_or(ApiError::NotFound)?;
    let state_dir = st.cfg.state_dir.clone().ok_or(ApiError::NotFound)?;
    let known: Option<[u8; 32]> = node.content_hash.as_deref().and_then(|h| h.try_into().ok());
    let size = q.s;
    let sd = state_dir.clone();
    let found = blocking(move || match known {
        Some(h) => thumbs::cached(&sd, &h, size),
        None => thumbs::Cached::Missing,
    })
    .await?;
    let made = match found {
        thumbs::Cached::Image(path, mime) => Some((
            blocking(move || std::fs::read(path))
                .await?
                .map_err(io_err)?,
            mime,
        )),
        thumbs::Cached::Failed => None,
        thumbs::Cached::Missing => {
            let _permit = st
                .thumbnails
                .acquire()
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let path = roots::dir(&data_dir, root).join(db::rel_path(&st.db, node.id).await?);
            blocking(move || make_thumbnail(&path, &state_dir, size))
                .await?
                .map_err(io_err)?
        }
    };
    let (bytes, mime) = made.ok_or(ApiError::NotFound)?;
    let cache = if q.v == Some(node.rev) {
        "private, max-age=31536000, immutable"
    } else {
        "private, no-cache"
    };
    Ok((
        [
            (axum::http::header::CONTENT_TYPE, mime),
            (axum::http::header::CACHE_CONTROL, cache),
        ],
        bytes,
    )
        .into_response())
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> ApiResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

fn io_err(e: std::io::Error) -> ApiError {
    if e.kind() == std::io::ErrorKind::NotFound {
        ApiError::NotFound
    } else {
        ApiError::Internal(format!("Vorschaubild: {e}"))
    }
}

/// Reads the file once, files the thumbnail under the hash of exactly those bytes.
fn make_thumbnail(
    path: &std::path::Path,
    state_dir: &std::path::Path,
    size: u32,
) -> std::io::Result<Option<(Vec<u8>, &'static str)>> {
    if std::fs::metadata(path)?.len() > thumbs::MAX_INPUT {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    let hash = xlrx_chunk::Chunker::new()
        .digest_slice(&bytes)
        .content
        .hash
        .0;
    match thumbs::cached(state_dir, &hash, size) {
        thumbs::Cached::Image(p, mime) => return Ok(Some((std::fs::read(p)?, mime))),
        thumbs::Cached::Failed => return Ok(None),
        thumbs::Cached::Missing => {}
    }
    match thumbs::render(&bytes, size) {
        Ok((thumb, mime)) => {
            thumbs::store(state_dir, &hash, size, Some((&thumb, mime)))?;
            Ok(Some((thumb, mime)))
        }
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "kein Vorschaubild möglich");
            thumbs::store(state_dir, &hash, size, None)?;
            Ok(None)
        }
    }
}

pub(crate) fn mtime(ms: Option<i64>) -> Option<OffsetDateTime> {
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
    let (folder, _) = editable(&st, &me, parent).await?;
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
    let (node, _) = editable(&st, &me, id).await?;
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

#[derive(Deserialize)]
pub struct DataClassReq {
    /// "cloud", "local", or "inherit" (as the folder above).
    pub class: String,
}

/// Sets the data class of a folder: whoever manages it, with a fresh second factor (PLAN 16.1).
pub async fn set_data_class(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<DataClassReq>,
) -> ApiResult<Json<NodeDetail>> {
    let a = access::require(&st.db, me.id, id, Role::Manager).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    if !a.node.is_dir() {
        return Err(ApiError::bad(
            "Die Datenklasse gilt für Ordner und alles darin.",
        ));
    }
    me.require_step_up()?;
    let class = match req.class.as_str() {
        "inherit" => None,
        c => Some(Class::parse(c).ok_or_else(|| ApiError::bad("Unbekannte Datenklasse."))?),
    };
    let default = st.cfg.default_data_class;
    let before = data_class::effective(&st.db, default, &[id])
        .await?
        .get(&id)
        .map(|e| e.class);
    data_class::set(&st.db, id, class, me.id).await?;
    // Now "Nur lokal": its contents leave the outside cache at once (and are never served again).
    mirror::revoke_forbidden(&st).await?;
    let after = data_class::effective(&st.db, default, &[id])
        .await?
        .get(&id)
        .map(|e| e.class);
    // Becoming "local" will also have to remove what cloud services produced for it (M4:
    // vectors, image descriptions); the log keeps the change either way.
    crate::audit::log(
        &st.db,
        Some(me.id),
        None,
        "data_class_changed",
        Some(&client.ip),
        serde_json::json!({ "node": id, "set": class, "before": before, "after": after }),
    )
    .await?;
    get_node(State(st), me, Path(id)).await
}

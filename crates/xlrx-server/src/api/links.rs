//! Public links (PLAN 9.2): managing them on an item, and everything a link opens for people
//! without an account (`/api/public/{token}/…`, the page `/s/{token}` of the web app).

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::header::{RANGE, SET_COOKIE};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use super::files::{self, ContentQuery, Crumb, NodeInfo, ThumbQuery};
use crate::audit;
use crate::auth::session::{ClientInfo, CurrentUser};
use crate::auth::throttle;
use crate::error::{ApiError, ApiResult};
use crate::files::access::{self, Role};
use crate::files::content::{self as file_content, Target};
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::files::links::{self, Kind, LINK_COLS, LinkRow, Open};
use crate::state::AppState;

#[derive(Serialize)]
pub struct LinkInfo {
    pub id: i64,
    /// The address to hand out (missing only if the server key changed since).
    pub url: Option<String>,
    pub kind: Kind,
    pub password: bool,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    pub expired: bool,
    pub max_downloads: Option<i32>,
    pub downloads: i32,
    pub created_by: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

async fn info(st: &AppState, row: &LinkRow) -> ApiResult<LinkInfo> {
    let by: Option<String> = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(row.created_by)
        .fetch_optional(&st.db)
        .await?;
    Ok(LinkInfo {
        id: row.id,
        url: links::token_of(st, row).map(|t| links::url(st, &t)),
        kind: row.kind(),
        password: row.password_hash.is_some(),
        expires_at: row.expires_at,
        expired: row.expired(),
        max_downloads: row.max_downloads,
        downloads: row.downloads,
        created_by: by,
        created_at: row.created_at,
    })
}

/// The links on an item (for whoever manages it).
pub async fn list(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Vec<LinkInfo>>> {
    let a = access::require(&st.db, me.id, id, Role::Manager).await?;
    let rows: Vec<LinkRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LINK_COLS} FROM links WHERE node_id = $1 ORDER BY created_at DESC"
    )))
    .bind(a.node.id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(info(&st, r).await?);
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct LinkReq {
    pub kind: String,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    #[serde(default)]
    pub max_downloads: Option<i32>,
}

/// A new link: whoever manages the item, with a fresh second factor (PLAN 16.1).
pub async fn create(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<LinkReq>,
) -> ApiResult<(StatusCode, Json<LinkInfo>)> {
    let a = access::require(&st.db, me.id, id, Role::Manager).await?;
    if a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    if a.node.parent_id.is_none() {
        return Err(ApiError::bad(
            "Eine ganze Ablage bekommt keinen Link – nur Ordner und Dateien darin.",
        ));
    }
    let kind = Kind::parse(&req.kind).ok_or_else(|| ApiError::bad("Unbekannte Art von Link."))?;
    if kind == Kind::Upload && !a.node.is_dir() {
        return Err(ApiError::bad("Hochladen geht nur in einen Ordner."));
    }
    if req
        .expires_at
        .is_some_and(|t| t <= OffsetDateTime::now_utc())
    {
        return Err(ApiError::bad("Das Ablaufdatum liegt in der Vergangenheit."));
    }
    if let Some(n) = req.max_downloads {
        if n < 1 {
            return Err(ApiError::bad("Mindestens ein Download."));
        }
        if !kind.download() {
            return Err(ApiError::bad("Dieser Link erlaubt keine Downloads."));
        }
    }
    let password = match req.password.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(p) if p.chars().count() < 8 => {
            return Err(ApiError::bad("Das Passwort braucht mindestens 8 Zeichen."));
        }
        Some(_) => req.password.clone(),
    };
    me.require_step_up()?;
    let password_hash = match password {
        Some(p) => {
            let _permit = st
                .hashing
                .acquire()
                .await
                .map_err(|e| ApiError::Internal(e.to_string()))?;
            let pw = st.clone();
            Some(
                tokio::task::spawn_blocking(move || pw.passwords.hash(&p))
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?
                    .map_err(ApiError::Internal)?,
            )
        }
        None => None,
    };
    let (token, hash) = links::new_token();
    let row: LinkRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO links (token_hash, token_sealed, node_id, kind, password_hash, expires_at,
                            max_downloads, created_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING {LINK_COLS}"
    )))
    .bind(&hash)
    .bind(links::seal(&st, &token, &hash))
    .bind(a.node.id)
    .bind(kind.as_str())
    .bind(&password_hash)
    .bind(req.expires_at)
    .bind(req.max_downloads)
    .bind(me.id)
    .fetch_one(&st.db)
    .await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "link_created",
        Some(&client.ip),
        json!({
            "link": row.id, "node": a.node.id, "kind": kind,
            "password": password_hash.is_some(), "expires_at": req.expires_at,
            "max_downloads": req.max_downloads,
        }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(info(&st, &row).await?)))
}

/// Ends a link at once (no second factor needed: it only takes access away).
pub async fn remove(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let row = links::by_id(&st.db, id).await?.ok_or(ApiError::NotFound)?;
    access::require(&st.db, me.id, row.node_id, Role::Manager).await?;
    sqlx::query("DELETE FROM links WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "link_removed",
        Some(&client.ip),
        json!({ "link": id, "node": row.node_id, "kind": row.kind() }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- Without an account ----

/// Every request through a link: rate limit and lockout per address, then the link itself.
/// Unknown tokens count as failed attempts, so guessing locks the address out.
async fn enter(st: &AppState, client: &ClientInfo, token: &str) -> ApiResult<Open> {
    st.public_limit.check(&client.ip)?;
    let ip = throttle::ip_key(&client.ip);
    throttle::check(&st.db, std::slice::from_ref(&ip)).await?;
    match links::open(&st.db, token).await {
        Err(ApiError::NotFound) => {
            throttle::fail(&st.db, &[ip]).await?;
            Err(ApiError::NotFound)
        }
        other => other,
    }
}

/// [`enter`], and unlocked if the link has a password.
async fn usable(
    st: &AppState,
    client: &ClientInfo,
    headers: &HeaderMap,
    token: &str,
) -> ApiResult<Open> {
    let o = enter(st, client, token).await?;
    if !links::unlocked(st, &o.link, headers) {
        return Err(ApiError::Unauthenticated(
            "password_required",
            "Dieser Link ist mit einem Passwort geschützt.".into(),
        ));
    }
    Ok(o)
}

#[derive(Serialize)]
pub struct Can {
    pub browse: bool,
    pub download: bool,
    pub upload: bool,
    pub replace: bool,
}

#[derive(Serialize)]
pub struct PublicInfo {
    pub kind: Kind,
    /// A password is needed first (nothing else is told until then).
    pub locked: bool,
    pub node: Option<NodeInfo>,
    pub owner: Option<String>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub expires_at: Option<OffsetDateTime>,
    pub can: Can,
    /// Largest file that may be uploaded through a link (bytes).
    pub max_upload: u64,
    /// Downloads left, if limited.
    pub downloads_left: Option<i32>,
}

pub async fn public_info(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path(token): Path<String>,
) -> ApiResult<Json<PublicInfo>> {
    let o = enter(&st, &client, &token).await?;
    let locked = !links::unlocked(&st, &o.link, &headers);
    let owner: Option<String> = if locked {
        None
    } else {
        sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
            .bind(o.link.created_by)
            .fetch_optional(&st.db)
            .await?
    };
    Ok(Json(PublicInfo {
        kind: o.kind,
        locked,
        node: (!locked).then(|| NodeInfo::from(&o.node)),
        owner,
        expires_at: o.link.expires_at,
        can: Can {
            browse: o.kind.browse(),
            download: o.kind.download(),
            upload: o.kind.upload() && o.node.is_dir(),
            replace: o.kind.replace(),
        },
        max_upload: st.cfg.link_upload_max,
        downloads_left: o.link.max_downloads.map(|m| (m - o.link.downloads).max(0)),
    }))
}

#[derive(Deserialize)]
pub struct UnlockReq {
    pub password: String,
}

/// Unlocks a link with its password: a cookie valid for this link only. Wrong passwords lock
/// out the link (after a few) and the address (after more).
pub async fn unlock(
    State(st): State<AppState>,
    client: ClientInfo,
    Path(token): Path<String>,
    Json(req): Json<UnlockReq>,
) -> ApiResult<Response> {
    let o = enter(&st, &client, &token).await?;
    let Some(stored) = o.link.password_hash.clone() else {
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let keys = [throttle::ip_key(&client.ip), format!("link:{}", o.link.id)];
    throttle::check(&st.db, &keys).await?;
    let ok = {
        let _permit = st
            .hashing
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let pw = st.clone();
        tokio::task::spawn_blocking(move || pw.passwords.verify(Some(&stored), &req.password))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
    };
    if !ok {
        throttle::fail(&st.db, &keys).await?;
        audit::log(
            &st.db,
            None,
            None,
            "link_password_failed",
            Some(&client.ip),
            json!({ "link": o.link.id, "node": o.node.id }),
        )
        .await?;
        return Err(ApiError::unauthorized("Falsches Passwort."));
    }
    throttle::reset(&st.db, &keys[1]).await?;
    audit::log(
        &st.db,
        None,
        None,
        "link_unlocked",
        Some(&client.ip),
        json!({ "link": o.link.id, "node": o.node.id }),
    )
    .await?;
    Ok((
        StatusCode::NO_CONTENT,
        [(SET_COOKIE, links::unlock_cookie(&st, &token, &o.link))],
    )
        .into_response())
}

#[derive(Serialize)]
pub struct PublicNode {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// From the link's item down to this one.
    pub path: Vec<Crumb>,
}

/// A node at or below the link's item.
async fn reach(st: &AppState, o: &Open, id: i64) -> ApiResult<(NodeRow, Vec<Crumb>)> {
    let (node, chain) = links::within(&st.db, o.node.id, id).await?;
    Ok((
        node,
        chain
            .into_iter()
            .map(|(id, name)| Crumb { id, name })
            .collect(),
    ))
}

fn may_browse(o: &Open) -> ApiResult<()> {
    if o.kind.browse() {
        Ok(())
    } else {
        Err(ApiError::forbidden(
            "Über diesen Link kann man nur hochladen.",
        ))
    }
}

pub async fn public_node(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path((token, id)): Path<(String, i64)>,
) -> ApiResult<Json<PublicNode>> {
    let o = usable(&st, &client, &headers, &token).await?;
    may_browse(&o)?;
    let (node, path) = reach(&st, &o, id).await?;
    Ok(Json(PublicNode {
        node: NodeInfo::from(&node),
        path,
    }))
}

pub async fn public_children(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path((token, id)): Path<(String, i64)>,
) -> ApiResult<Json<Vec<NodeInfo>>> {
    let o = usable(&st, &client, &headers, &token).await?;
    may_browse(&o)?;
    let (node, _) = reach(&st, &o, id).await?;
    if !node.is_dir() {
        return Err(ApiError::bad("Kein Ordner."));
    }
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE parent_id = $1 AND deleted_at IS NULL
          ORDER BY kind, name_folded LIMIT 10000"
    )))
    .bind(node.id)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows.iter().map(NodeInfo::from).collect()))
}

/// Whether a request starts a download (not the continuation of one): those are counted.
fn starts_download(headers: &HeaderMap) -> bool {
    match headers.get(RANGE).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(r) => r
            .trim()
            .strip_prefix("bytes=")
            .is_some_and(|r| r.trim_start().starts_with("0-")),
    }
}

pub async fn public_content(
    State(st): State<AppState>,
    client: ClientInfo,
    Path((token, id)): Path<(String, i64)>,
    Query(q): Query<ContentQuery>,
    req: Request,
) -> ApiResult<Response> {
    let o = usable(&st, &client, req.headers(), &token).await?;
    may_browse(&o)?;
    let (node, _) = reach(&st, &o, id).await?;
    if node.is_dir() {
        return Err(ApiError::bad("Ordner können nicht heruntergeladen werden."));
    }
    let inline = q.inline && files::inline_allowed(&files::mime_of(&node.name));
    if !inline {
        if !o.kind.download() {
            return Err(ApiError::forbidden("Dieser Link erlaubt nur das Ansehen."));
        }
        if starts_download(req.headers()) {
            if !links::count_download(&st.db, o.link.id).await? {
                return Err(ApiError::Gone(
                    "Die erlaubte Zahl an Downloads ist erreicht.".into(),
                ));
            }
            audit::log(
                &st.db,
                None,
                None,
                "link_downloaded",
                Some(&client.ip),
                json!({ "link": o.link.id, "node": node.id }),
            )
            .await?;
        }
    }
    let data_dir = st.cfg.data_dir.as_deref().ok_or(ApiError::NotFound)?;
    let path =
        crate::files::roots::dir(data_dir, &o.root).join(db::rel_path(&st.db, node.id).await?);
    files::serve(req, &path, &node.name, inline, node.content_hash.as_deref()).await
}

pub async fn public_thumbnail(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path((token, id)): Path<(String, i64)>,
    Query(q): Query<ThumbQuery>,
) -> ApiResult<Response> {
    let o = usable(&st, &client, &headers, &token).await?;
    may_browse(&o)?;
    let (node, _) = reach(&st, &o, id).await?;
    files::thumbnail_of(&st, &node, &o.root, &q).await
}

#[derive(Deserialize)]
pub struct PublicUpload {
    pub name: String,
    /// Required: nothing larger than allowed is received at all.
    pub size: u64,
    pub mtime_ms: Option<i64>,
}

fn size_ok(st: &AppState, size: u64) -> ApiResult<()> {
    if size > st.cfg.link_upload_max {
        return Err(ApiError::bad(format!(
            "Über einen Link gehen Dateien bis {} MB.",
            st.cfg.link_upload_max / 1_000_000
        )));
    }
    Ok(())
}

/// A new file through a link; a name in use gets a free one ("Name (1).ext"), nothing is
/// overwritten.
pub async fn public_upload(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path((token, id)): Path<(String, i64)>,
    Query(q): Query<PublicUpload>,
    body: axum::body::Body,
) -> ApiResult<Response> {
    let o = usable(&st, &client, &headers, &token).await?;
    if !o.kind.upload() {
        return Err(ApiError::forbidden("Dieser Link erlaubt kein Hochladen."));
    }
    // Upload-only links only ever add to the folder itself (nothing below it is known to them).
    let (folder, _) = if o.kind.browse() {
        reach(&st, &o, id).await?
    } else if id == o.node.id {
        (o.node.clone(), Vec::new())
    } else {
        return Err(ApiError::NotFound);
    };
    if !folder.is_dir() {
        return Err(ApiError::bad("Kein Ordner."));
    }
    crate::files::ops::valid_name(&q.name)?;
    size_ok(&st, q.size)?;
    let staged = file_content::stage(&st, body, Some(q.size)).await?;
    let (node, _) = file_content::write(
        &st,
        o.link.created_by,
        Target::New {
            parent_id: folder.id,
            name: q.name.clone(),
            keep_both: true,
        },
        staged,
        files::mtime(q.mtime_ms),
    )
    .await?;
    audit::log(
        &st.db,
        None,
        None,
        "link_uploaded",
        Some(&client.ip),
        json!({ "link": o.link.id, "node": node.id, "name": node.name, "size": node.size }),
    )
    .await?;
    if o.kind.browse() {
        Ok((StatusCode::CREATED, Json(NodeInfo::from(&node))).into_response())
    } else {
        // Without seeing the folder, not even the name it got there is told.
        Ok((StatusCode::CREATED, Json(json!({ "name": q.name }))).into_response())
    }
}

#[derive(Deserialize)]
pub struct PublicReplace {
    pub base_rev: i64,
    pub size: u64,
    pub mtime_ms: Option<i64>,
}

/// New contents of a file through an edit link; the previous one stays as a version.
pub async fn public_replace(
    State(st): State<AppState>,
    client: ClientInfo,
    headers: HeaderMap,
    Path((token, id)): Path<(String, i64)>,
    Query(q): Query<PublicReplace>,
    body: axum::body::Body,
) -> ApiResult<Json<NodeInfo>> {
    let o = usable(&st, &client, &headers, &token).await?;
    if !o.kind.replace() {
        return Err(ApiError::forbidden("Dieser Link erlaubt keine Änderungen."));
    }
    let (node, _) = reach(&st, &o, id).await?;
    if node.is_dir() {
        return Err(ApiError::bad("Ordner haben keinen Inhalt."));
    }
    if node.rev != q.base_rev {
        return Err(ApiError::Conflict(format!(
            "„{}“ wurde inzwischen geändert. Bitte neu laden; beide Fassungen bleiben so erhalten.",
            node.name
        )));
    }
    size_ok(&st, q.size)?;
    let staged = file_content::stage(&st, body, Some(q.size)).await?;
    let (node, _) = file_content::write(
        &st,
        o.link.created_by,
        Target::Replace {
            node_id: node.id,
            base_rev: q.base_rev,
        },
        staged,
        files::mtime(q.mtime_ms),
    )
    .await?;
    audit::log(
        &st.db,
        None,
        None,
        "link_replaced",
        Some(&client.ip),
        json!({ "link": o.link.id, "node": node.id, "rev": node.rev }),
    )
    .await?;
    Ok(Json(NodeInfo::from(&node)))
}

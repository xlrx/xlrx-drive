//! Administration: create accounts (with setup link), reset second factors, disable accounts.
//! Every modifying action requires admin rights and a fresh second factor.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;

use crate::audit;
use crate::auth::device;
use crate::auth::session::{ClientInfo, CurrentUser};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::users;

#[derive(Serialize)]
pub struct UserInfo {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub disabled: bool,
    pub totp: bool,
    pub passkeys: i64,
    /// Setup not yet completed (no password or no second factor).
    pub setup_pending: bool,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

pub async fn list_users(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<UserInfo>>> {
    me.require_admin()?;
    let counts: Vec<(i64, i64)> =
        sqlx::query_as("SELECT user_id, count(*) FROM passkeys GROUP BY user_id")
            .fetch_all(&st.db)
            .await?;
    let out = users::all(&st.db)
        .await?
        .into_iter()
        .map(|u| {
            let passkeys = counts.iter().find(|(id, _)| *id == u.id).map_or(0, |c| c.1);
            let totp = u.totp_secret_enc.is_some();
            UserInfo {
                id: u.id,
                setup_pending: u.password_hash.is_none() || (!totp && passkeys == 0),
                username: u.username,
                display_name: u.display_name,
                email: u.email,
                is_admin: u.is_admin,
                disabled: u.disabled_at.is_some(),
                totp,
                passkeys,
                created_at: u.created_at,
            }
        })
        .collect();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct CreateUserReq {
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    #[serde(default)]
    pub is_admin: bool,
}

#[derive(Serialize)]
pub struct SetupLinkResp {
    pub user_id: i64,
    pub setup_url: String,
    pub valid_hours: i64,
}

pub async fn create_user(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<CreateUserReq>,
) -> ApiResult<Json<SetupLinkResp>> {
    me.require_admin()?;
    me.require_step_up()?;
    let u = users::create(
        &st.db,
        &req.username,
        &req.display_name,
        req.email.as_deref(),
        req.is_admin,
    )
    .await?;
    let token = users::create_invite(&st.db, u.id, Some(me.id)).await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(u.id),
        "user_created",
        Some(&client.ip),
        json!({"username": u.username, "is_admin": u.is_admin}),
    )
    .await?;
    Ok(Json(SetupLinkResp {
        user_id: u.id,
        setup_url: users::setup_url(&st, &token),
        valid_hours: users::INVITE_HOURS,
    }))
}

/// New setup link, e.g. when the old one has expired or the password was forgotten.
pub async fn invite(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<Json<SetupLinkResp>> {
    me.require_admin()?;
    me.require_step_up()?;
    let u = users::by_id_required(&st.db, id).await?;
    let token = users::create_invite(&st.db, u.id, Some(me.id)).await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(u.id),
        "invite_created",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(Json(SetupLinkResp {
        user_id: u.id,
        setup_url: users::setup_url(&st, &token),
        valid_hours: users::INVITE_HOURS,
    }))
}

/// Reset all second factors (e.g. phone lost, no codes left). Ends all sessions; the person sets
/// up again via the new link.
pub async fn reset_factors(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<Json<SetupLinkResp>> {
    me.require_admin()?;
    me.require_step_up()?;
    let u = users::by_id_required(&st.db, id).await?;
    reset_user_factors(&st.db, u.id).await?;
    let token = users::create_invite(&st.db, u.id, Some(me.id)).await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(u.id),
        "factors_reset",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(Json(SetupLinkResp {
        user_id: u.id,
        setup_url: users::setup_url(&st, &token),
        valid_hours: users::INVITE_HOURS,
    }))
}

pub async fn reset_user_factors(db: &sqlx::PgPool, user_id: i64) -> ApiResult<()> {
    let mut tx = db.begin().await?;
    sqlx::query("UPDATE users SET totp_secret_enc = NULL, totp_last_step = NULL, updated_at = now() WHERE id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    // Fixed table names, no user input.
    for table in ["passkeys", "recovery_codes", "sessions", "login_challenges"] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE user_id = $1"
        )))
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    }
    device::revoke_all(&mut tx, user_id, "factors_reset").await?;
    tx.commit().await?;
    Ok(())
}

#[derive(Deserialize)]
pub struct DisableReq {
    pub disabled: bool,
}

pub async fn set_disabled(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<DisableReq>,
) -> ApiResult<Json<serde_json::Value>> {
    me.require_admin()?;
    me.require_step_up()?;
    if id == me.id {
        return Err(ApiError::bad(
            "Das eigene Konto kann nicht gesperrt werden.",
        ));
    }
    let u = users::by_id_required(&st.db, id).await?;
    sqlx::query(
        "UPDATE users SET disabled_at = CASE WHEN $2 THEN now() ELSE NULL END, updated_at = now() WHERE id = $1",
    )
    .bind(u.id)
    .bind(req.disabled)
    .execute(&st.db)
    .await?;
    if req.disabled {
        let mut tx = st.db.begin().await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = $1")
            .bind(u.id)
            .execute(&mut *tx)
            .await?;
        device::revoke_all(&mut tx, u.id, "user_disabled").await?;
        tx.commit().await?;
    }
    let action = if req.disabled {
        "user_disabled"
    } else {
        "user_enabled"
    };
    audit::log(
        &st.db,
        Some(me.id),
        Some(u.id),
        action,
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(Json(json!({ "disabled": req.disabled })))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    pub limit: Option<i64>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct AuditEntry {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub actor: Option<String>,
    pub target: Option<String>,
    pub action: String,
    pub ip: Option<String>,
    pub details: serde_json::Value,
}

pub async fn audit_log(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<Vec<AuditEntry>>> {
    me.require_admin()?;
    let limit = q.limit.unwrap_or(200).clamp(1, 1000);
    let rows = sqlx::query_as(
        "SELECT a.id, a.at, ua.username AS actor, ut.username AS target, a.action, a.ip, a.details
           FROM audit_log a
           LEFT JOIN users ua ON ua.id = a.actor_user_id
           LEFT JOIN users ut ON ut.id = a.target_user_id
          ORDER BY a.id DESC LIMIT $1",
    )
    .bind(limit)
    .fetch_all(&st.db)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize)]
pub struct SearchStatus {
    /// The index is running.
    pub running: bool,
    /// Last journal entry, and how far the index got.
    pub journal: i64,
    pub indexed: i64,
    /// Contents with an extracted text.
    pub texts: i64,
    pub jobs: crate::jobs::Counts,
    /// What text extraction can read (`None`: not running).
    pub extract: Option<crate::extract::Status>,
    /// Why jobs failed or wait (most recent first).
    pub problems: Vec<JobProblem>,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct JobProblem {
    pub state: String,
    pub error: Option<String>,
    pub n: i64,
}

/// How far search and text extraction are (PLAN 6.6: progress in the admin area).
pub async fn search_status(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<SearchStatus>> {
    me.require_admin()?;
    let (journal, texts): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT coalesce(max(seq), 0) FROM journal), (SELECT count(*) FROM content_text)",
    )
    .fetch_one(&st.db)
    .await?;
    let problems = sqlx::query_as(
        "SELECT state, last_error AS error, count(*) AS n FROM jobs
          WHERE state IN ('failed', 'waiting')
          GROUP BY state, last_error ORDER BY max(run_after) DESC LIMIT 10",
    )
    .fetch_all(&st.db)
    .await?;
    let search = st.search.get();
    Ok(Json(SearchStatus {
        running: search.is_some(),
        journal,
        indexed: search.map_or(0, |s| s.indexed()),
        texts,
        jobs: crate::jobs::counts(&st.db).await?,
        extract: st.extract.get().map(|r| r.status),
        problems,
    }))
}

/// Gives jobs that failed for good another round of attempts.
pub async fn retry_jobs(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
) -> ApiResult<Json<serde_json::Value>> {
    me.require_admin()?;
    me.require_step_up()?;
    let n = crate::jobs::retry_failed(&st.db).await?;
    st.jobs_wake.notify_one();
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "jobs_retried",
        Some(&client.ip),
        json!({ "jobs": n }),
    )
    .await?;
    Ok(Json(json!({ "retried": n })))
}

#[derive(Serialize)]
pub struct GroupOut {
    pub id: i64,
    pub name: String,
    pub members: Vec<MemberOut>,
}

#[derive(Serialize)]
pub struct MemberOut {
    pub id: i64,
    pub name: String,
}

#[derive(Deserialize)]
pub struct GroupReq {
    pub name: String,
    #[serde(default)]
    pub members: Vec<i64>,
}

/// Groups such as "Familie" or "Eltern", for sharing with several people at once.
pub async fn list_groups(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<GroupOut>>> {
    me.require_admin()?;
    let groups: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, name FROM groups ORDER BY lower(name)")
            .fetch_all(&st.db)
            .await?;
    let mut out = Vec::with_capacity(groups.len());
    for (id, name) in groups {
        let members: Vec<(i64, String)> = sqlx::query_as(
            "SELECT u.id, u.display_name FROM group_members m JOIN users u ON u.id = m.user_id
              WHERE m.group_id = $1 ORDER BY lower(u.display_name)",
        )
        .bind(id)
        .fetch_all(&st.db)
        .await?;
        out.push(GroupOut {
            id,
            name,
            members: members
                .into_iter()
                .map(|(id, name)| MemberOut { id, name })
                .collect(),
        });
    }
    Ok(Json(out))
}

async fn save_group(st: &AppState, id: Option<i64>, req: &GroupReq) -> ApiResult<i64> {
    let name = req.name.trim();
    if name.is_empty() || name.chars().count() > 80 {
        return Err(ApiError::bad("Bitte einen Namen für die Gruppe angeben."));
    }
    let mut tx = st.db.begin().await?;
    let id: i64 = match id {
        None => sqlx::query_scalar(
            "INSERT INTO groups (name, name_folded) VALUES ($1, $2)
             ON CONFLICT (name_folded) DO NOTHING RETURNING id",
        )
        .bind(name)
        .bind(users::fold(name))
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::Conflict(format!("Die Gruppe „{name}“ gibt es schon.")))?,
        Some(id) => {
            let r = sqlx::query("UPDATE groups SET name = $2, name_folded = $3 WHERE id = $1")
                .bind(id)
                .bind(name)
                .bind(users::fold(name))
                .execute(&mut *tx)
                .await
                .map_err(|e| match e {
                    sqlx::Error::Database(d) if d.is_unique_violation() => {
                        ApiError::Conflict(format!("Die Gruppe „{name}“ gibt es schon."))
                    }
                    e => e.into(),
                })?;
            if r.rows_affected() == 0 {
                return Err(ApiError::NotFound);
            }
            id
        }
    };
    sqlx::query("DELETE FROM group_members WHERE group_id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    let r = sqlx::query(
        "INSERT INTO group_members (group_id, user_id)
         SELECT $1, u.id FROM users u WHERE u.id = ANY($2)",
    )
    .bind(id)
    .bind(&req.members)
    .execute(&mut *tx)
    .await?;
    let mut wanted = req.members.clone();
    wanted.sort_unstable();
    wanted.dedup();
    if r.rows_affected() as usize != wanted.len() {
        return Err(ApiError::bad("Unbekanntes Konto in der Gruppe."));
    }
    tx.commit().await?;
    Ok(id)
}

pub async fn create_group(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<GroupReq>,
) -> ApiResult<(axum::http::StatusCode, Json<serde_json::Value>)> {
    me.require_admin()?;
    me.require_step_up()?;
    let id = save_group(&st, None, &req).await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "group_created",
        Some(&client.ip),
        json!({ "group": id, "name": req.name.trim(), "members": req.members }),
    )
    .await?;
    Ok((axum::http::StatusCode::CREATED, Json(json!({ "id": id }))))
}

pub async fn update_group(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<GroupReq>,
) -> ApiResult<axum::http::StatusCode> {
    me.require_admin()?;
    me.require_step_up()?;
    save_group(&st, Some(id), &req).await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "group_changed",
        Some(&client.ip),
        json!({ "group": id, "name": req.name.trim(), "members": req.members }),
    )
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

/// Removes a group; what was shared with it is no longer shared with its members.
pub async fn delete_group(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<axum::http::StatusCode> {
    me.require_admin()?;
    me.require_step_up()?;
    let r = sqlx::query("DELETE FROM groups WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "group_deleted",
        Some(&client.ip),
        json!({ "group": id }),
    )
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct SpaceOut {
    pub id: i64,
    pub name: String,
    pub path: String,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scanned_at: Option<OffsetDateTime>,
    pub members: Vec<SpaceMember>,
}

#[derive(Serialize, Deserialize)]
pub struct SpaceMember {
    #[serde(flatten)]
    pub to: super::shares::Principal,
    #[serde(default, skip_deserializing)]
    pub name: String,
    pub role: String,
}

#[derive(Deserialize)]
pub struct SpaceReq {
    pub name: String,
    /// Below the data directory, e.g. "Familie" (only when mounting).
    pub path: Option<String>,
    #[serde(default)]
    pub members: Vec<SpaceMember>,
}

/// Shared roots ("Geteilte Ablagen") with their members.
pub async fn list_spaces(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<SpaceOut>>> {
    me.require_admin()?;
    let roots: Vec<(i64, String, String, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT id, name, rel_path, scanned_at FROM roots WHERE kind = 'space' ORDER BY lower(name)",
    )
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(roots.len());
    for (id, name, path, scanned_at) in roots {
        let rows: Vec<(Option<i64>, Option<i64>, String, String)> = sqlx::query_as(
            "SELECT m.user_id, m.group_id, m.role, coalesce(u.display_name, g.name)
               FROM root_members m
               LEFT JOIN users u ON u.id = m.user_id
               LEFT JOIN groups g ON g.id = m.group_id
              WHERE m.root_id = $1 ORDER BY 4",
        )
        .bind(id)
        .fetch_all(&st.db)
        .await?;
        let members = rows
            .into_iter()
            .filter_map(|(u, g, role, name)| {
                let to = match (u, g) {
                    (Some(u), None) => super::shares::Principal::User(u),
                    (None, Some(g)) => super::shares::Principal::Group(g),
                    _ => return None,
                };
                Some(SpaceMember { to, name, role })
            })
            .collect();
        out.push(SpaceOut {
            id,
            name,
            path,
            scanned_at,
            members,
        });
    }
    Ok(Json(out))
}

async fn set_members(st: &AppState, root_id: i64, members: &[SpaceMember]) -> ApiResult<()> {
    let mut tx = st.db.begin().await?;
    sqlx::query("DELETE FROM root_members WHERE root_id = $1")
        .bind(root_id)
        .execute(&mut *tx)
        .await?;
    for m in members {
        let role = crate::files::access::Role::parse(&m.role)
            .ok_or_else(|| ApiError::bad("Unbekannte Rolle."))?;
        let (u, g) = match m.to {
            super::shares::Principal::User(u) => (Some(u), None),
            super::shares::Principal::Group(g) => (None, Some(g)),
        };
        sqlx::query(
            "INSERT INTO root_members (root_id, user_id, group_id, role) VALUES ($1, $2, $3, $4)
             ON CONFLICT (root_id, user_id, group_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(root_id)
        .bind(u)
        .bind(g)
        .bind(role.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|e| match e {
            sqlx::Error::Database(d) if d.is_foreign_key_violation() => {
                ApiError::bad("Unbekanntes Konto oder unbekannte Gruppe.")
            }
            e => e.into(),
        })?;
    }
    tx.commit().await?;
    Ok(())
}

/// Mounts an existing folder of the NAS (e.g. a Synology Drive team folder) as a shared root.
pub async fn create_space(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<SpaceReq>,
) -> ApiResult<(axum::http::StatusCode, Json<serde_json::Value>)> {
    me.require_admin()?;
    me.require_step_up()?;
    let path = req
        .path
        .as_deref()
        .ok_or_else(|| ApiError::bad("Bitte den Ordner auf dem NAS angeben."))?;
    let root = crate::files::roots::mount_space(&st, &req.name, path, me.id).await?;
    set_members(&st, root.id, &req.members).await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "space_mounted",
        Some(&client.ip),
        json!({ "root": root.id, "name": root.name, "path": root.rel_path, "members": req.members.len() }),
    )
    .await?;
    // Watch first, then import what is there (as for a home on first access).
    let st2 = st.clone();
    let r = root.clone();
    tokio::spawn(async move {
        if st2.cfg.watch
            && let Err(e) = crate::files::watch::start(&st2, &r).await
        {
            tracing::warn!(root = r.id, error = ?e, "Überwachung nicht gestartet");
        }
        if let Err(e) = crate::files::roots::scan(&st2, &r).await {
            tracing::warn!(root = r.id, error = ?e, "Erster Abgleich fehlgeschlagen");
        }
    });
    Ok((
        axum::http::StatusCode::CREATED,
        Json(json!({ "id": root.id })),
    ))
}

/// Renames a shared root and sets its members.
pub async fn update_space(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
    Json(req): Json<SpaceReq>,
) -> ApiResult<axum::http::StatusCode> {
    me.require_admin()?;
    me.require_step_up()?;
    let name = req.name.trim();
    if name.is_empty() || name.contains('/') {
        return Err(ApiError::bad(
            "Bitte einen Namen ohne Schrägstrich angeben.",
        ));
    }
    let r = sqlx::query("UPDATE roots SET name = $2 WHERE id = $1 AND kind = 'space'")
        .bind(id)
        .bind(name)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    set_members(&st, id, &req.members).await?;
    audit::log(
        &st.db,
        Some(me.id),
        None,
        "space_changed",
        Some(&client.ip),
        json!({ "root": id, "name": name, "members": req.members.len() }),
    )
    .await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

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

//! Own account: manage factors, recovery codes, password, sessions.
//! Anything that changes factors or the password requires a fresh second factor (step-up).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use time::OffsetDateTime;
use webauthn_rs::prelude::RegisterPublicKeyCredential;

use super::setup::{
    PasskeyRegBeginResp, TotpSetupResp, finish_registration, start_registration, totp_setup_resp,
};
use crate::audit;
use crate::auth::ceremony::Ceremony;
use crate::auth::secret::totp_aad;
use crate::auth::session::{ClientInfo, CurrentUser};
use crate::auth::totp;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::users::{self, Me};

#[derive(Serialize)]
pub struct MeResp {
    #[serde(flatten)]
    pub me: Me,
    pub step_up_valid: bool,
}

pub async fn get_me(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<MeResp>> {
    Ok(Json(MeResp {
        me: users::me(&st.db, me.id).await?,
        step_up_valid: me.step_up_valid(),
    }))
}

#[derive(Deserialize)]
pub struct PasswordReq {
    pub password: String,
}

pub async fn change_password(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<PasswordReq>,
) -> ApiResult<StatusCode> {
    me.require_step_up()?;
    st.passwords
        .check_policy(&req.password, &me.username)
        .map_err(ApiError::BadRequest)?;
    let hash = st.hash_password(req.password).await?;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(me.id)
        .bind(hash)
        .execute(&st.db)
        .await?;
    // End other sessions: anyone who knew the old password is locked out afterwards.
    sqlx::query("DELETE FROM sessions WHERE user_id = $1 AND id <> $2")
        .bind(me.id)
        .bind(me.session_id)
        .execute(&st.db)
        .await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "password_changed",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct TotpBeginResp {
    pub ceremony: String,
    #[serde(flatten)]
    pub setup: TotpSetupResp,
}

pub async fn totp_begin(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<TotpBeginResp>> {
    me.require_step_up()?;
    let secret = totp::new_secret();
    let setup = totp_setup_resp(&secret, &me.username);
    let ceremony = st.ceremonies.insert(Ceremony::TotpEnroll {
        user_id: me.id,
        secret,
    });
    Ok(Json(TotpBeginResp { ceremony, setup }))
}

#[derive(Deserialize)]
pub struct TotpConfirmReq {
    pub ceremony: String,
    pub code: String,
}

pub async fn totp_confirm(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<TotpConfirmReq>,
) -> ApiResult<StatusCode> {
    me.require_step_up()?;
    let Some(Ceremony::TotpEnroll { user_id, secret }) = st.ceremonies.take(&req.ceremony) else {
        return Err(ApiError::unauthorized(
            "Der Vorgang ist abgelaufen. Bitte neu beginnen.",
        ));
    };
    if user_id != me.id {
        return Err(ApiError::forbidden("Falsches Konto."));
    }
    let Some(step) = totp::verify(&secret, &req.code, users::unix_now(), None) else {
        return Err(ApiError::unauthorized(
            "Der Code ist ungültig. Bitte die Einrichtung neu beginnen.",
        ));
    };
    sqlx::query(
        "UPDATE users SET totp_secret_enc = $2, totp_last_step = $3, updated_at = now() WHERE id = $1",
    )
    .bind(me.id)
    .bind(st.secrets.seal(&totp_aad(me.id), &secret))
    .bind(step)
    .execute(&st.db)
    .await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "totp_enabled",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn totp_remove(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
) -> ApiResult<StatusCode> {
    me.require_step_up()?;
    if users::passkeys(&st.db, me.id).await?.is_empty() {
        return Err(ApiError::Conflict(
            "Die Authenticator-App ist der einzige zweite Faktor. Erst einen Passkey hinzufügen."
                .into(),
        ));
    }
    sqlx::query("UPDATE users SET totp_secret_enc = NULL, totp_last_step = NULL, updated_at = now() WHERE id = $1")
        .bind(me.id)
        .execute(&st.db)
        .await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "totp_removed",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct PasskeyBeginReq {
    #[serde(default)]
    pub name: String,
}

pub async fn passkey_begin(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<PasskeyBeginReq>,
) -> ApiResult<Json<PasskeyRegBeginResp>> {
    me.require_step_up()?;
    let user = users::by_id_required(&st.db, me.id).await?;
    Ok(Json(start_registration(&st, &user, req.name, None).await?))
}

#[derive(Deserialize)]
pub struct PasskeyFinishReq {
    pub ceremony: String,
    pub credential: RegisterPublicKeyCredential,
}

pub async fn passkey_finish(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<PasskeyFinishReq>,
) -> ApiResult<StatusCode> {
    me.require_step_up()?;
    let name = finish_registration(&st, me.id, &req.ceremony, &req.credential, None).await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "passkey_added",
        Some(&client.ip),
        json!({"name": name}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RenameReq {
    pub name: String,
}

pub async fn passkey_rename(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
    Json(req): Json<RenameReq>,
) -> ApiResult<StatusCode> {
    let name: String = req.name.trim().chars().take(60).collect();
    if name.is_empty() {
        return Err(ApiError::bad("Der Name darf nicht leer sein."));
    }
    let r = sqlx::query("UPDATE passkeys SET name = $3 WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(me.id)
        .bind(name)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn passkey_remove(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    me.require_step_up()?;
    let user = users::by_id_required(&st.db, me.id).await?;
    let pks = users::passkeys(&st.db, me.id).await?;
    if !pks.iter().any(|p| p.id == id) {
        return Err(ApiError::NotFound);
    }
    if user.totp_secret_enc.is_none() && pks.len() <= 1 {
        return Err(ApiError::Conflict(
            "Das ist der einzige zweite Faktor. Erst die Authenticator-App oder einen weiteren Passkey einrichten.".into(),
        ));
    }
    sqlx::query("DELETE FROM passkeys WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(me.id)
        .execute(&st.db)
        .await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "passkey_removed",
        Some(&client.ip),
        json!({"id": id}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
pub struct RecoveryResp {
    pub recovery_codes: Vec<String>,
}

pub async fn recovery_regenerate(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
) -> ApiResult<Json<RecoveryResp>> {
    me.require_step_up()?;
    let mut tx = st.db.begin().await?;
    let codes = users::replace_recovery_codes(&mut tx, me.id).await?;
    tx.commit().await?;
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "recovery_codes_regenerated",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    Ok(Json(RecoveryResp {
        recovery_codes: codes,
    }))
}

#[derive(Serialize, sqlx::FromRow)]
pub struct SessionInfo {
    pub id: i64,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    pub user_agent: Option<String>,
    pub ip: Option<String>,
    #[sqlx(skip)]
    pub current: bool,
}

pub async fn sessions(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<Vec<SessionInfo>>> {
    let mut list: Vec<SessionInfo> = sqlx::query_as(
        "SELECT id, created_at, last_seen_at, user_agent, ip FROM sessions
          WHERE user_id = $1 AND expires_at > now() ORDER BY last_seen_at DESC",
    )
    .bind(me.id)
    .fetch_all(&st.db)
    .await?;
    for s in &mut list {
        s.current = s.id == me.session_id;
    }
    Ok(Json(list))
}

pub async fn session_revoke(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM sessions WHERE id = $1 AND user_id = $2")
        .bind(id)
        .bind(me.id)
        .execute(&st.db)
        .await?;
    if r.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

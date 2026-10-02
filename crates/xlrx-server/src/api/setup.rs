//! Einrichtung über einen Einladungs- bzw. Einrichtungslink: Passwort festlegen, dann mindestens einen
//! zweiten Faktor (TOTP oder Passkey). Erst danach gibt es eine Sitzung und Wiederherstellungscodes.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::header::SET_COOKIE;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use webauthn_rs::prelude::{CreationChallengeResponse, RegisterPublicKeyCredential};

use crate::audit;
use crate::auth::ceremony::Ceremony;
use crate::auth::session::{self, ClientInfo};
use crate::auth::throttle::{self, ip_key};
use crate::auth::tokens::{hash_token, new_token};
use crate::auth::{secret::totp_aad, totp};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::users::{self, UserRow};

const SETUP_MINUTES: i32 = 30;
const MAX_ATTEMPTS: i32 = 10;

fn setup_totp_aad(user_id: i64) -> Vec<u8> {
    format!("xlrx totp pending v1 user {user_id}").into_bytes()
}

#[derive(Deserialize)]
pub struct StartReq {
    pub invite: String,
}

#[derive(Serialize)]
pub struct StartResp {
    pub setup_token: String,
    pub username: String,
    pub display_name: String,
    pub has_password: bool,
}

pub async fn start(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<StartReq>,
) -> ApiResult<Json<StartResp>> {
    let keys = [ip_key(&client.ip)];
    throttle::check(&st.db, &keys).await?;
    let invite_hash = hash_token(req.invite.trim());
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT user_id FROM invites WHERE token_hash = $1 AND used_at IS NULL AND expires_at > now()",
    )
    .bind(&invite_hash)
    .fetch_optional(&st.db)
    .await?;
    let user = match row {
        Some((id,)) => users::by_id(&st.db, id)
            .await?
            .filter(|u| u.disabled_at.is_none()),
        None => None,
    };
    let Some(user) = user else {
        throttle::fail(&st.db, &keys).await?;
        return Err(ApiError::unauthorized(
            "Der Einrichtungslink ist ungültig oder abgelaufen.",
        ));
    };
    let (token, hash) = new_token();
    sqlx::query(
        "INSERT INTO login_challenges (token_hash, user_id, purpose, invite_hash, expires_at)
         VALUES ($1, $2, 'setup', $3, now() + make_interval(mins => $4))",
    )
    .bind(hash)
    .bind(user.id)
    .bind(&invite_hash)
    .bind(SETUP_MINUTES)
    .execute(&st.db)
    .await?;
    Ok(Json(StartResp {
        setup_token: token,
        username: user.username,
        display_name: user.display_name,
        has_password: user.password_hash.is_some(),
    }))
}

struct Setup {
    hash: Vec<u8>,
    user: UserRow,
    invite_hash: Option<Vec<u8>>,
    pending_totp_enc: Option<Vec<u8>>,
}

#[derive(sqlx::FromRow)]
struct SetupRow {
    user_id: i64,
    invite_hash: Option<Vec<u8>>,
    pending_totp_enc: Option<Vec<u8>>,
    attempts: i32,
}

async fn setup(st: &AppState, token: &str) -> ApiResult<Setup> {
    let hash = hash_token(token);
    let row: Option<SetupRow> = sqlx::query_as(
        "SELECT user_id, invite_hash, pending_totp_enc, attempts FROM login_challenges
          WHERE token_hash = $1 AND purpose = 'setup' AND expires_at > now()",
    )
    .bind(&hash)
    .fetch_optional(&st.db)
    .await?;
    let expired =
        || ApiError::unauthorized("Die Einrichtung ist abgelaufen. Bitte den Link erneut öffnen.");
    let row = row.ok_or_else(expired)?;
    if row.attempts > MAX_ATTEMPTS {
        return Err(expired());
    }
    let user = users::by_id(&st.db, row.user_id)
        .await?
        .filter(|u| u.disabled_at.is_none())
        .ok_or_else(expired)?;
    Ok(Setup {
        hash,
        user,
        invite_hash: row.invite_hash,
        pending_totp_enc: row.pending_totp_enc,
    })
}

#[derive(Deserialize)]
pub struct PasswordReq {
    pub setup_token: String,
    pub password: String,
}

pub async fn password(
    State(st): State<AppState>,
    Json(req): Json<PasswordReq>,
) -> ApiResult<StatusCode> {
    let s = setup(&st, &req.setup_token).await?;
    st.passwords
        .check_policy(&req.password, &s.user.username)
        .map_err(ApiError::BadRequest)?;
    let hash = st.hash_password(req.password).await?;
    sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
        .bind(s.user.id)
        .bind(hash)
        .execute(&st.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct TokenReq {
    pub setup_token: String,
}

#[derive(Serialize)]
pub struct TotpSetupResp {
    pub otpauth_url: String,
    pub secret: String,
    pub qr_svg: String,
}

pub fn totp_setup_resp(secret: &[u8], username: &str) -> TotpSetupResp {
    let url = totp::otpauth_url(secret, username);
    TotpSetupResp {
        qr_svg: totp::qr_svg(&url),
        secret: totp::secret_base32(secret),
        otpauth_url: url,
    }
}

pub async fn totp_begin(
    State(st): State<AppState>,
    Json(req): Json<TokenReq>,
) -> ApiResult<Json<TotpSetupResp>> {
    let s = setup(&st, &req.setup_token).await?;
    let secret = totp::new_secret();
    let enc = st.secrets.seal(&setup_totp_aad(s.user.id), &secret);
    sqlx::query("UPDATE login_challenges SET pending_totp_enc = $2 WHERE token_hash = $1")
        .bind(&s.hash)
        .bind(enc)
        .execute(&st.db)
        .await?;
    Ok(Json(totp_setup_resp(&secret, &s.user.username)))
}

#[derive(Deserialize)]
pub struct TotpConfirmReq {
    pub setup_token: String,
    pub code: String,
}

fn require_password(user: &UserRow) -> ApiResult<()> {
    if user.password_hash.is_none() {
        return Err(ApiError::bad("Bitte zuerst ein Passwort festlegen."));
    }
    Ok(())
}

pub async fn totp_confirm(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<TotpConfirmReq>,
) -> ApiResult<Response> {
    let s = setup(&st, &req.setup_token).await?;
    require_password(&s.user)?;
    let secret = s
        .pending_totp_enc
        .as_deref()
        .and_then(|enc| st.secrets.open(&setup_totp_aad(s.user.id), enc))
        .ok_or_else(|| {
            ApiError::bad("Bitte zuerst die Einrichtung der Authenticator-App starten.")
        })?;
    let Some(step) = totp::verify(&secret, &req.code, users::unix_now(), None) else {
        sqlx::query("UPDATE login_challenges SET attempts = attempts + 1 WHERE token_hash = $1")
            .bind(&s.hash)
            .execute(&st.db)
            .await?;
        return Err(ApiError::unauthorized(
            "Der Code ist ungültig. Stimmt die Uhrzeit auf dem Gerät?",
        ));
    };
    sqlx::query(
        "UPDATE users SET totp_secret_enc = $2, totp_last_step = $3, updated_at = now() WHERE id = $1",
    )
    .bind(s.user.id)
    .bind(st.secrets.seal(&totp_aad(s.user.id), &secret))
    .bind(step)
    .execute(&st.db)
    .await?;
    audit::log(
        &st.db,
        Some(s.user.id),
        Some(s.user.id),
        "totp_enabled",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    finish(&st, &s, &client).await
}

#[derive(Deserialize)]
pub struct PasskeyBeginReq {
    pub setup_token: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Serialize)]
pub struct PasskeyRegBeginResp {
    pub ceremony: String,
    pub options: CreationChallengeResponse,
}

/// Registrierung eines Passkeys starten (bei der Einrichtung oder angemeldet).
pub async fn start_registration(
    st: &AppState,
    user: &UserRow,
    name: String,
    setup: Option<Vec<u8>>,
) -> ApiResult<PasskeyRegBeginResp> {
    let exclude: Vec<_> = users::passkeys(&st.db, user.id)
        .await?
        .into_iter()
        .map(|p| p.passkey.0.cred_id().clone())
        .collect();
    let (options, state) = st
        .webauthn
        .start_passkey_registration(user.uuid, &user.username, &user.display_name, Some(exclude))
        .map_err(|e| ApiError::Internal(format!("Passkey: {e}")))?;
    let ceremony = st.ceremonies.insert(Ceremony::Register {
        user_id: user.id,
        name,
        setup,
        state,
    });
    Ok(PasskeyRegBeginResp { ceremony, options })
}

/// Registrierung abschließen und speichern. Liefert den Namen des Passkeys.
pub async fn finish_registration(
    st: &AppState,
    user_id: i64,
    ceremony: &str,
    credential: &RegisterPublicKeyCredential,
    setup: Option<&[u8]>,
) -> ApiResult<String> {
    let expired = || ApiError::unauthorized("Der Vorgang ist abgelaufen. Bitte erneut versuchen.");
    let Some(Ceremony::Register {
        user_id: uid,
        name,
        setup: s,
        state,
    }) = st.ceremonies.take(ceremony)
    else {
        return Err(expired());
    };
    if uid != user_id || s.as_deref() != setup {
        return Err(expired());
    }
    let pk = st
        .webauthn
        .finish_passkey_registration(credential, &state)
        .map_err(|e| {
            ApiError::bad(format!(
                "Der Passkey konnte nicht registriert werden ({e})."
            ))
        })?;
    users::add_passkey(&st.db, user_id, &name, &pk).await?;
    Ok(name)
}

pub async fn passkey_begin(
    State(st): State<AppState>,
    Json(req): Json<PasskeyBeginReq>,
) -> ApiResult<Json<PasskeyRegBeginResp>> {
    let s = setup(&st, &req.setup_token).await?;
    require_password(&s.user)?;
    Ok(Json(
        start_registration(&st, &s.user, req.name, Some(s.hash)).await?,
    ))
}

#[derive(Deserialize)]
pub struct PasskeyFinishReq {
    pub setup_token: String,
    pub ceremony: String,
    pub credential: RegisterPublicKeyCredential,
}

pub async fn passkey_finish(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<PasskeyFinishReq>,
) -> ApiResult<Response> {
    let s = setup(&st, &req.setup_token).await?;
    require_password(&s.user)?;
    let name = finish_registration(
        &st,
        s.user.id,
        &req.ceremony,
        &req.credential,
        Some(&s.hash),
    )
    .await?;
    audit::log(
        &st.db,
        Some(s.user.id),
        Some(s.user.id),
        "passkey_added",
        Some(&client.ip),
        json!({"name": name}),
    )
    .await?;
    finish(&st, &s, &client).await
}

#[derive(Serialize)]
struct FinishResp {
    recovery_codes: Vec<String>,
    me: users::Me,
}

/// Einrichtung abschließen: Link verbrauchen, alte Sitzungen beenden, Wiederherstellungscodes erzeugen,
/// anmelden.
async fn finish(st: &AppState, s: &Setup, client: &ClientInfo) -> ApiResult<Response> {
    let mut tx = st.db.begin().await?;
    if let Some(invite) = &s.invite_hash {
        let r = sqlx::query(
            "UPDATE invites SET used_at = now() WHERE token_hash = $1 AND used_at IS NULL",
        )
        .bind(invite)
        .execute(&mut *tx)
        .await?;
        if r.rows_affected() != 1 {
            return Err(ApiError::unauthorized(
                "Der Einrichtungslink wurde bereits verwendet.",
            ));
        }
    }
    sqlx::query("DELETE FROM login_challenges WHERE user_id = $1")
        .bind(s.user.id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM sessions WHERE user_id = $1")
        .bind(s.user.id)
        .execute(&mut *tx)
        .await?;
    let codes = users::replace_recovery_codes(&mut tx, s.user.id).await?;
    tx.commit().await?;
    audit::log(
        &st.db,
        Some(s.user.id),
        Some(s.user.id),
        "setup_completed",
        Some(&client.ip),
        json!({}),
    )
    .await?;
    let token = session::create(&st.db, s.user.id, client).await?;
    let me = users::me(&st.db, s.user.id).await?;
    Ok((
        [(SET_COOKIE, session::cookie(st, &token))],
        Json(FinishResp {
            recovery_codes: codes,
            me,
        }),
    )
        .into_response())
}

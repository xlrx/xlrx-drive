//! Anmeldung: Passwort → TOTP/Wiederherstellungscode/Passkey, oder Passkey allein; Abmelden; Step-up.

use axum::Json;
use axum::extract::State;
use axum::http::header::SET_COOKIE;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use webauthn_rs::prelude::{
    AuthenticationResult, Passkey, PublicKeyCredential, RequestChallengeResponse,
};

use crate::audit;
use crate::auth::ceremony::{AuthPurpose, Ceremony};
use crate::auth::session::{self, ClientInfo, CurrentUser};
use crate::auth::throttle::{self, ip_key, name_key};
use crate::auth::tokens::{hash_token, new_token};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::users::{self, UserRow};

const SECOND_FACTOR_MINUTES: i32 = 5;
const MAX_ATTEMPTS: i32 = 5;

#[derive(Deserialize)]
pub struct LoginReq {
    pub username: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct LoginResp {
    /// Zwischenschritt-Token für den zweiten Faktor (5 Minuten gültig).
    pub challenge: String,
    pub totp: bool,
    pub passkey: bool,
}

pub async fn login(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<LoginReq>,
) -> ApiResult<Json<LoginResp>> {
    let folded = users::fold(&req.username);
    let keys = [ip_key(&client.ip), name_key(&folded)];
    throttle::check(&st.db, &keys).await?;
    if req.password.len() > 4096 || folded.len() > 256 {
        return Err(ApiError::bad("Eingabe zu lang."));
    }
    let user = users::by_folded(&st.db, &folded).await?;
    let stored = user.as_ref().and_then(|u| u.password_hash.clone());
    let ok = st.verify_password(stored, req.password).await?;
    let user = match user {
        Some(u) if ok && u.disabled_at.is_none() => u,
        other => {
            let locked = throttle::fail(&st.db, &keys).await?;
            let target = other.as_ref().map(|u| u.id);
            if target.is_some() {
                audit::log(
                    &st.db,
                    None,
                    target,
                    "login_failed",
                    Some(&client.ip),
                    json!({"reason": "password"}),
                )
                .await?;
            }
            if locked {
                audit::log(
                    &st.db,
                    None,
                    target,
                    "login_locked",
                    Some(&client.ip),
                    json!({"name": folded}),
                )
                .await?;
            }
            return Err(ApiError::unauthorized(
                "Benutzername oder Passwort ist falsch.",
            ));
        }
    };
    let has_passkey = !users::passkeys(&st.db, user.id).await?.is_empty();
    let has_totp = user.totp_secret_enc.is_some();
    if !has_totp && !has_passkey {
        return Err(ApiError::forbidden(
            "Für dieses Konto ist noch kein zweiter Faktor eingerichtet. Bitte den Einrichtungslink verwenden.",
        ));
    }
    let (token, hash) = new_token();
    sqlx::query(
        "INSERT INTO login_challenges (token_hash, user_id, purpose, expires_at)
         VALUES ($1, $2, 'second_factor', now() + make_interval(mins => $3))",
    )
    .bind(hash)
    .bind(user.id)
    .bind(SECOND_FACTOR_MINUTES)
    .execute(&st.db)
    .await?;
    Ok(Json(LoginResp {
        challenge: token,
        totp: has_totp,
        passkey: has_passkey,
    }))
}

/// Zwischenschritt nach dem Passwort. Jeder Versuch zählt; nach 5 Fehlversuchen verfällt er.
async fn second_factor(st: &AppState, token: &str) -> ApiResult<(Vec<u8>, UserRow)> {
    let hash = hash_token(token);
    let row: Option<(i64, i32)> = sqlx::query_as(
        "UPDATE login_challenges SET attempts = attempts + 1
          WHERE token_hash = $1 AND purpose = 'second_factor' AND expires_at > now()
          RETURNING user_id, attempts",
    )
    .bind(&hash)
    .fetch_optional(&st.db)
    .await?;
    let expired = || ApiError::unauthorized("Die Anmeldung ist abgelaufen. Bitte neu beginnen.");
    let (user_id, attempts) = row.ok_or_else(expired)?;
    if attempts > MAX_ATTEMPTS {
        delete_challenge(st, &hash).await?;
        return Err(expired());
    }
    let user = users::by_id(&st.db, user_id).await?.ok_or_else(expired)?;
    if user.disabled_at.is_some() {
        return Err(expired());
    }
    Ok((hash, user))
}

async fn delete_challenge(st: &AppState, hash: &[u8]) -> ApiResult<()> {
    sqlx::query("DELETE FROM login_challenges WHERE token_hash = $1")
        .bind(hash)
        .execute(&st.db)
        .await?;
    Ok(())
}

/// Anmeldung abgeschlossen: Sitzung anlegen, Cookie setzen.
pub async fn complete_login(
    st: &AppState,
    user: &UserRow,
    client: &ClientInfo,
    method: &str,
) -> ApiResult<Response> {
    let token = session::create(&st.db, user.id, client).await?;
    throttle::reset(&st.db, &name_key(&user.username_folded)).await?;
    audit::log(
        &st.db,
        Some(user.id),
        Some(user.id),
        "login",
        Some(&client.ip),
        json!({"method": method}),
    )
    .await?;
    let me = users::me(&st.db, user.id).await?;
    Ok(([(SET_COOKIE, session::cookie(st, &token))], Json(me)).into_response())
}

#[derive(Deserialize)]
pub struct CodeReq {
    pub challenge: String,
    pub code: String,
}

pub async fn totp(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<CodeReq>,
) -> ApiResult<Response> {
    let (hash, user) = second_factor(&st, &req.challenge).await?;
    let keys = [ip_key(&client.ip), name_key(&user.username_folded)];
    throttle::check(&st.db, &keys).await?;
    if !users::verify_totp(&st, &user, &req.code).await? {
        throttle::fail(&st.db, &keys).await?;
        audit::log(
            &st.db,
            None,
            Some(user.id),
            "login_failed",
            Some(&client.ip),
            json!({"reason": "totp"}),
        )
        .await?;
        return Err(ApiError::unauthorized("Der Code ist ungültig."));
    }
    delete_challenge(&st, &hash).await?;
    complete_login(&st, &user, &client, "totp").await
}

pub async fn recovery(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<CodeReq>,
) -> ApiResult<Response> {
    let (hash, user) = second_factor(&st, &req.challenge).await?;
    let keys = [ip_key(&client.ip), name_key(&user.username_folded)];
    throttle::check(&st.db, &keys).await?;
    if !users::consume_recovery_code(&st.db, user.id, &req.code).await? {
        throttle::fail(&st.db, &keys).await?;
        audit::log(
            &st.db,
            None,
            Some(user.id),
            "login_failed",
            Some(&client.ip),
            json!({"reason": "recovery_code"}),
        )
        .await?;
        return Err(ApiError::unauthorized(
            "Der Wiederherstellungscode ist ungültig.",
        ));
    }
    delete_challenge(&st, &hash).await?;
    let left = users::recovery_left(&st.db, user.id).await?;
    audit::log(
        &st.db,
        Some(user.id),
        Some(user.id),
        "recovery_code_used",
        Some(&client.ip),
        json!({"left": left}),
    )
    .await?;
    complete_login(&st, &user, &client, "recovery_code").await
}

#[derive(Deserialize)]
pub struct PasskeyBeginReq {
    /// Anmeldung nur mit Passkey.
    pub username: Option<String>,
    /// Oder: Passkey als zweiter Faktor nach dem Passwort.
    pub challenge: Option<String>,
}

#[derive(Serialize)]
pub struct PasskeyBeginResp {
    pub ceremony: String,
    pub options: RequestChallengeResponse,
}

fn no_passkey() -> ApiError {
    ApiError::bad("Anmeldung mit Passkey ist für dieses Konto nicht möglich.")
}

async fn start_auth(
    st: &AppState,
    user: &UserRow,
    purpose: AuthPurpose,
) -> ApiResult<PasskeyBeginResp> {
    let keys: Vec<Passkey> = users::passkeys(&st.db, user.id)
        .await?
        .into_iter()
        .map(|p| p.passkey.0)
        .collect();
    if keys.is_empty() {
        return Err(no_passkey());
    }
    let (options, state) = st
        .webauthn
        .start_passkey_authentication(&keys)
        .map_err(|e| ApiError::Internal(format!("Passkey: {e}")))?;
    let ceremony = st.ceremonies.insert(Ceremony::Authenticate {
        user_id: user.id,
        purpose,
        state,
    });
    Ok(PasskeyBeginResp { ceremony, options })
}

pub async fn passkey_begin(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<PasskeyBeginReq>,
) -> ApiResult<Json<PasskeyBeginResp>> {
    throttle::check(&st.db, &[ip_key(&client.ip)]).await?;
    let (user, purpose) = if let Some(ch) = req.challenge {
        let hash = hash_token(&ch);
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT user_id FROM login_challenges
              WHERE token_hash = $1 AND purpose = 'second_factor' AND expires_at > now()",
        )
        .bind(&hash)
        .fetch_optional(&st.db)
        .await?;
        let user = match row {
            Some((id,)) => users::by_id(&st.db, id).await?,
            None => None,
        };
        let user = user.ok_or_else(|| {
            ApiError::unauthorized("Die Anmeldung ist abgelaufen. Bitte neu beginnen.")
        })?;
        (user, AuthPurpose::SecondFactor(hash))
    } else if let Some(name) = req.username {
        let folded = users::fold(&name);
        throttle::check(&st.db, &[name_key(&folded)]).await?;
        let user = users::by_folded(&st.db, &folded)
            .await?
            .filter(|u| u.disabled_at.is_none())
            .ok_or_else(no_passkey)?;
        (user, AuthPurpose::Login)
    } else {
        return Err(ApiError::bad("Benutzername fehlt."));
    };
    Ok(Json(start_auth(&st, &user, purpose).await?))
}

#[derive(Deserialize)]
pub struct PasskeyFinishReq {
    pub ceremony: String,
    pub credential: PublicKeyCredential,
}

/// Prüft eine Passkey-Antwort und aktualisiert Zähler und letzte Nutzung.
async fn finish_auth(
    st: &AppState,
    ceremony: &str,
    credential: &PublicKeyCredential,
    client: &ClientInfo,
    want_step_up: Option<i64>,
) -> ApiResult<(UserRow, AuthPurpose)> {
    let expired = || ApiError::unauthorized("Der Vorgang ist abgelaufen. Bitte erneut versuchen.");
    let Some(Ceremony::Authenticate {
        user_id,
        purpose,
        state,
    }) = st.ceremonies.take(ceremony)
    else {
        return Err(expired());
    };
    match (&purpose, want_step_up) {
        (AuthPurpose::StepUp(s), Some(w)) if *s == w => {}
        (AuthPurpose::StepUp(_), _) | (_, Some(_)) => return Err(expired()),
        _ => {}
    }
    let user = users::by_id(&st.db, user_id)
        .await?
        .filter(|u| u.disabled_at.is_none())
        .ok_or_else(expired)?;
    let keys = [ip_key(&client.ip), name_key(&user.username_folded)];
    throttle::check(&st.db, &keys).await?;
    let res = match st
        .webauthn
        .finish_passkey_authentication(credential, &state)
    {
        Ok(r) => r,
        Err(e) => {
            throttle::fail(&st.db, &keys).await?;
            audit::log(
                &st.db,
                None,
                Some(user.id),
                "login_failed",
                Some(&client.ip),
                json!({"reason": "passkey", "detail": e.to_string()}),
            )
            .await?;
            return Err(ApiError::unauthorized(
                "Der Passkey konnte nicht bestätigt werden.",
            ));
        }
    };
    if !res.user_verified() {
        return Err(ApiError::unauthorized(
            "Der Passkey wurde ohne Nutzerprüfung verwendet.",
        ));
    }
    update_passkey(st, user.id, &res).await?;
    Ok((user, purpose))
}

async fn update_passkey(st: &AppState, user_id: i64, res: &AuthenticationResult) -> ApiResult<()> {
    let row: Option<(i64, sqlx::types::Json<Passkey>)> = sqlx::query_as(
        "SELECT id, passkey FROM passkeys WHERE user_id = $1 AND credential_id = $2",
    )
    .bind(user_id)
    .bind(res.cred_id().as_ref().to_vec())
    .fetch_optional(&st.db)
    .await?;
    let Some((id, sqlx::types::Json(mut pk))) = row else {
        return Err(ApiError::unauthorized("Unbekannter Passkey."));
    };
    pk.update_credential(res);
    sqlx::query("UPDATE passkeys SET passkey = $2, last_used_at = now() WHERE id = $1")
        .bind(id)
        .bind(sqlx::types::Json(&pk))
        .execute(&st.db)
        .await?;
    Ok(())
}

pub async fn passkey_finish(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<PasskeyFinishReq>,
) -> ApiResult<Response> {
    let (user, purpose) = finish_auth(&st, &req.ceremony, &req.credential, &client, None).await?;
    if let AuthPurpose::SecondFactor(hash) = purpose {
        let r = sqlx::query(
            "DELETE FROM login_challenges WHERE token_hash = $1 AND user_id = $2 AND expires_at > now()",
        )
        .bind(&hash)
        .bind(user.id)
        .execute(&st.db)
        .await?;
        if r.rows_affected() != 1 {
            return Err(ApiError::unauthorized(
                "Die Anmeldung ist abgelaufen. Bitte neu beginnen.",
            ));
        }
    }
    complete_login(&st, &user, &client, "passkey").await
}

pub async fn logout(State(st): State<AppState>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = session::token_from_headers(&headers) {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(hash_token(&token))
            .execute(&st.db)
            .await?;
    }
    Ok((
        [(SET_COOKIE, session::clear_cookie(&st))],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct StepUpTotpReq {
    pub code: String,
}

pub async fn step_up_totp(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<StepUpTotpReq>,
) -> ApiResult<StatusCode> {
    let user = users::by_id_required(&st.db, me.id).await?;
    let keys = [ip_key(&client.ip), name_key(&user.username_folded)];
    throttle::check(&st.db, &keys).await?;
    if !users::verify_totp(&st, &user, &req.code).await? {
        throttle::fail(&st.db, &keys).await?;
        audit::log(
            &st.db,
            Some(me.id),
            Some(me.id),
            "step_up_failed",
            Some(&client.ip),
            json!({"method": "totp"}),
        )
        .await?;
        return Err(ApiError::unauthorized("Der Code ist ungültig."));
    }
    session::mark_step_up(&st.db, me.session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn step_up_passkey_begin(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Json<PasskeyBeginResp>> {
    let user = users::by_id_required(&st.db, me.id).await?;
    Ok(Json(
        start_auth(&st, &user, AuthPurpose::StepUp(me.session_id)).await?,
    ))
}

pub async fn step_up_passkey_finish(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Json(req): Json<PasskeyFinishReq>,
) -> ApiResult<StatusCode> {
    let (user, _) = finish_auth(
        &st,
        &req.ceremony,
        &req.credential,
        &client,
        Some(me.session_id),
    )
    .await?;
    if user.id != me.id {
        return Err(ApiError::forbidden("Falsches Konto."));
    }
    session::mark_step_up(&st.db, me.session_id).await?;
    Ok(StatusCode::NO_CONTENT)
}

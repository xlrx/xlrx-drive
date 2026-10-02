//! Web sessions: random token in the cookie (`HttpOnly`, `Secure`, `SameSite=Strict`), in the DB
//! only as a hash. Idle timeout 8 h, maximum lifetime 7 days. Sensitive actions require a fresh
//! second factor (step-up, 10 min).
//!
//! Devices (Mac, iPhone) send an access token instead (`Authorization: Bearer …`, see
//! [`super::device`]); a request with one is judged by the token alone, never by a cookie.

use axum::extract::FromRequestParts;
use axum::http::HeaderValue;
use axum::http::header::{AUTHORIZATION, COOKIE, USER_AGENT};
use axum::http::request::Parts;
use sqlx::PgPool;
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::tokens::{hash_token, new_token};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const COOKIE_NAME: &str = "xlrx_session";
pub const IDLE: Duration = Duration::hours(8);
pub const MAX_AGE: Duration = Duration::days(7);
pub const STEP_UP_VALID: Duration = Duration::minutes(10);

/// Signed-in person (from the session cookie or a device's access token).
#[derive(Clone, Debug)]
pub struct CurrentUser {
    pub id: i64,
    pub uuid: Uuid,
    pub username: String,
    pub display_name: String,
    pub is_admin: bool,
    /// Signed in through the browser …
    pub session_id: Option<i64>,
    /// … or with a device (never both).
    pub device_id: Option<i64>,
    /// Only sessions confirm a second factor again; devices never count as stepped up.
    pub step_up_at: Option<OffsetDateTime>,
}

impl CurrentUser {
    /// The browser session; actions on the account itself are not possible from a device.
    pub fn session(&self) -> ApiResult<i64> {
        self.session_id
            .ok_or_else(|| ApiError::forbidden("Nur im Browser möglich."))
    }

    pub fn step_up_valid(&self) -> bool {
        self.step_up_at
            .is_some_and(|t| OffsetDateTime::now_utc() - t < STEP_UP_VALID)
    }

    /// Sensitive action: requires a fresh second factor.
    pub fn require_step_up(&self) -> ApiResult<()> {
        if self.step_up_valid() {
            Ok(())
        } else {
            Err(ApiError::Forbidden("step_up_required".into()))
        }
    }

    pub fn require_admin(&self) -> ApiResult<()> {
        if self.is_admin {
            Ok(())
        } else {
            Err(ApiError::forbidden("Nur für Administratoren."))
        }
    }
}

/// IP and user agent of the request.
#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub ip: String,
    pub user_agent: Option<String>,
}

impl FromRequestParts<AppState> for ClientInfo {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|c| c.0.ip().to_string());
        // Behind Caddy: the last entry of X-Forwarded-For comes from the proxy itself.
        let forwarded = state
            .cfg
            .trust_proxy
            .then(|| {
                parts
                    .headers
                    .get("x-forwarded-for")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.rsplit(',').next())
                    .map(|s| s.trim().to_owned())
            })
            .flatten();
        Ok(Self {
            ip: forwarded.or(peer).unwrap_or_else(|| "unbekannt".into()),
            user_agent: parts
                .headers
                .get(USER_AGENT)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.chars().take(300).collect()),
        })
    }
}

pub fn token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE_NAME)
        .map(|(_, v)| v.to_owned())
}

#[derive(sqlx::FromRow)]
struct SessionRow {
    session_id: i64,
    last_seen_at: OffsetDateTime,
    step_up_at: Option<OffsetDateTime>,
    id: i64,
    uuid: Uuid,
    username: String,
    display_name: String,
    is_admin: bool,
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        if let Some(auth) = parts.headers.get(AUTHORIZATION) {
            let token = auth
                .to_str()
                .ok()
                .and_then(|v| v.strip_prefix("Bearer "))
                .map(str::to_owned)
                .ok_or_else(|| ApiError::unauthorized("Ungültige Anmeldung."))?;
            let client = ClientInfo::from_request_parts(parts, state)
                .await
                .unwrap_or_else(|e| match e {});
            return super::device::authenticate(state, token.trim(), &client).await;
        }
        let token = token_from_headers(&parts.headers)
            .ok_or_else(|| ApiError::unauthorized("Nicht angemeldet."))?;
        let row: Option<SessionRow> = sqlx::query_as(
            "SELECT s.id AS session_id, s.last_seen_at, s.step_up_at,
                    u.id, u.uuid, u.username, u.display_name, u.is_admin
               FROM sessions s JOIN users u ON u.id = s.user_id
              WHERE s.token_hash = $1 AND s.expires_at > now()
                AND s.last_seen_at > now() - make_interval(secs => $2)
                AND u.disabled_at IS NULL",
        )
        .bind(hash_token(&token))
        .bind(IDLE.whole_seconds() as f64)
        .fetch_optional(&state.db)
        .await?;
        let row = row.ok_or_else(|| ApiError::unauthorized("Nicht angemeldet."))?;
        if OffsetDateTime::now_utc() - row.last_seen_at > Duration::minutes(1) {
            sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE id = $1")
                .bind(row.session_id)
                .execute(&state.db)
                .await?;
        }
        Ok(Self {
            id: row.id,
            uuid: row.uuid,
            username: row.username,
            display_name: row.display_name,
            is_admin: row.is_admin,
            session_id: Some(row.session_id),
            device_id: None,
            step_up_at: row.step_up_at,
        })
    }
}

/// New session after a complete sign-in. The sign-in itself counts as a fresh second factor.
pub async fn create(db: &PgPool, user_id: i64, client: &ClientInfo) -> ApiResult<String> {
    let (token, hash) = new_token();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, expires_at, step_up_at, user_agent, ip)
         VALUES ($1, $2, now() + make_interval(secs => $3), now(), $4, $5)",
    )
    .bind(hash)
    .bind(user_id)
    .bind(MAX_AGE.whole_seconds() as f64)
    .bind(&client.user_agent)
    .bind(&client.ip)
    .execute(db)
    .await?;
    Ok(token)
}

pub async fn mark_step_up(db: &PgPool, session_id: i64) -> ApiResult<()> {
    sqlx::query("UPDATE sessions SET step_up_at = now() WHERE id = $1")
        .bind(session_id)
        .execute(db)
        .await?;
    Ok(())
}

pub fn cookie(state: &AppState, token: &str) -> HeaderValue {
    let secure = if state.cfg.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        MAX_AGE.whole_seconds()
    ))
    .expect("gültiger Header")
}

pub fn clear_cookie(state: &AppState) -> HeaderValue {
    let secure = if state.cfg.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}"
    ))
    .expect("gültiger Header")
}

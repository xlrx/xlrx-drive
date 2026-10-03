//! API errors: always JSON `{"error": "…"}` with a matching status. Internal details
//! only end up in the log.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    /// Not signed in, with a reason a device can act on (`token_invalid`: refresh, `revoked` or
    /// `reauth_required`: sign in again through the browser).
    Unauthenticated(&'static str, String),
    Forbidden(String),
    NotFound,
    Conflict(String),
    /// Existed, but no more (an expired link).
    Gone(String),
    /// A precondition of a change does not hold: sync clients get the reason, people the message.
    Rejected(xlrx_sync::Reject, String),
    TooManyRequests,
    /// A part of the server is not set up or not running (e.g. search without a state directory).
    Unavailable(String),
    Internal(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::Unauthorized(msg.into())
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::Forbidden(msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            Self::NotFound => (StatusCode::NOT_FOUND, "Nicht gefunden".into()),
            Self::Conflict(m) => (StatusCode::CONFLICT, m),
            Self::Gone(m) => (StatusCode::GONE, m),
            Self::Unauthenticated(reason, m) => {
                return (
                    StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({ "error": m, "reason": reason })),
                )
                    .into_response();
            }
            Self::Rejected(reason, m) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({ "error": m, "reason": reason })),
                )
                    .into_response();
            }
            Self::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "Zu viele Versuche. Bitte später erneut versuchen.".into(),
            ),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            Self::Internal(detail) => {
                tracing::error!(%detail, "interner Fehler");
                (StatusCode::INTERNAL_SERVER_ERROR, "Interner Fehler".into())
            }
        };
        (status, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(format!("Datenbank: {e}"))
    }
}

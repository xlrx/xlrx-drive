//! Fehler der API: immer JSON `{"error": "…"}` mit passendem Status. Interne Details landen nur im Log.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound,
    Conflict(String),
    TooManyRequests,
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
            Self::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "Zu viele Versuche. Bitte später erneut versuchen.".into(),
            ),
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

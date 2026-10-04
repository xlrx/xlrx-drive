//! Signing in a device (`POST /api/devices/token`).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// `POST /api/devices/token`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
pub enum TokenRequest {
    /// The code from the browser sign-in, with the PKCE verifier.
    AuthorizationCode {
        code: String,
        code_verifier: String,
        redirect_uri: String,
        /// Set when an existing device confirms again: its current refresh token.
        #[serde(skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
    },
    RefreshToken {
        refresh_token: String,
    },
}

/// What a device gets after signing in or refreshing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until the access token expires.
    pub expires_in: i64,
    pub device_id: i64,
    /// From then on the device needs a second factor again.
    #[serde(with = "time::serde::rfc3339")]
    pub confirm_until: OffsetDateTime,
}

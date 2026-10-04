//! Error answers: `{"error": "…"}`, with a `reason` where a client can act on it.
//!
//! Not every error is JSON: rejections of the web framework itself (a body that is not JSON, a
//! missing content type, a malformed query) answer with plain text and status 400, 415 or 422.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    /// Message for people (German).
    pub error: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The values of `reason`.
pub mod reason {
    /// 401: the access token is unknown or expired — refresh once. From the refresh endpoint:
    /// the refresh token is unknown — signed out, never retry.
    pub const TOKEN_INVALID: &str = "token_invalid";
    /// 401 from the refresh endpoint: the device was revoked.
    pub const REVOKED: &str = "revoked";
    /// 401 from the refresh endpoint: sign in again in the browser (second factor).
    pub const REAUTH_REQUIRED: &str = "reauth_required";
    /// 401 from the token endpoint: the sign-in code is invalid or expired.
    pub const CODE_INVALID: &str = "code_invalid";
    /// 409 of `POST /api/sync/ops`: upload the content first, then send the same operation again.
    pub const CONTENT_MISSING: &str = "content_missing";
    /// 409 of `POST /api/sync/ops`: the operation ID was used for a different operation.
    pub const OP_MISMATCH: &str = "op_mismatch";
    /// 409 of the feed or an operation: the cursor's journal entry is gone or different (the
    /// server's database was restored): rebuild.
    pub const CURSOR_INVALID: &str = "cursor_invalid";
    /// 409: the NAS is full.
    pub const DISK_FULL: &str = "disk_full";
}

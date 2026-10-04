//! Resumable uploads in parts (`/api/uploads/…`).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

/// What an upload becomes when it is committed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UploadTarget {
    /// A new file in a folder.
    New {
        parent_id: i64,
        name: String,
        #[serde(default)]
        keep_both: bool,
    },
    /// New content for a file, based on the revision the client knows.
    Replace { node_id: i64, base_rev: i64 },
    /// Content for a later sync operation (like `PUT /api/sync/content/{hash}`).
    Content,
}

/// `POST /api/uploads`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateUpload {
    pub size: u64,
    pub target: UploadTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<i64>,
}

/// An upload session (answer of `POST /api/uploads` and `GET /api/uploads/{id}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadInfo {
    pub id: Uuid,
    pub size: i64,
    /// Ranges `[start, end)` that arrived, in order.
    pub received: Vec<[i64; 2]>,
    pub state: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

/// `PUT /api/uploads/{id}/parts?offset=` (at most 8 MiB per part).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartQuery {
    pub offset: u64,
}

/// `POST /api/uploads/{id}/commit`
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitUpload {
    /// Content hash (xlrx-content-v1, hex) the client computed; checked when given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_both: Option<bool>,
}

/// Answer of the commit for [`UploadTarget::Content`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentCommitted {
    pub hash: String,
}

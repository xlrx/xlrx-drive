//! The roots a person can see (`GET /api/roots`).

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// A person's role over a whole root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Editor,
    Manager,
    Owner,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootInfo {
    /// Used as `root` in the change feed.
    pub id: i64,
    pub kind: String,
    pub name: String,
    /// The root's own node: `remote_root` of the sync engine.
    pub node_id: i64,
    pub role: Role,
    #[serde(with = "time::serde::rfc3339::option")]
    pub scanned_at: Option<OffsetDateTime>,
}

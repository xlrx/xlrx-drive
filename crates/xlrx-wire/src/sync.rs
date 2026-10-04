//! Change feed, live notifications, content ahead of operations, and the operations themselves
//! (`/api/sync/…`).

use serde::{Deserialize, Serialize};
use xlrx_proto::NodeId;
use xlrx_sync::{RemoteEntry, RemoteOp, RemoteResult};

/// `GET /api/sync/changes?root=&cursor=&limit=&check=`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangesQuery {
    pub root: i64,
    /// Last cursor received; 0 for the first fetch.
    #[serde(default)]
    pub cursor: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// `cursor_tag` received with `cursor`; a mismatch answers 409 `cursor_invalid`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// One node's current state; `None` if it was deleted or is no longer visible.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub node: NodeId,
    pub state: Option<RemoteEntry>,
}

/// Answer of `GET /api/sync/changes`. All pages until `more` is false belong together.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub cursor: i64,
    pub more: bool,
    /// Fingerprint of the journal entry at `cursor` (`None` at cursor 0): sent back as `check`.
    pub cursor_tag: Option<String>,
}

/// Payload of the server-sent event `change` (`GET /api/sync/notify`): a list of these.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changed {
    pub root: i64,
    pub seq: i64,
}

/// `PUT /api/sync/content/{hash}?size=`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

/// Answer of `GET /api/sync/content/{hash}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Available {
    pub available: bool,
}

/// `POST /api/sync/ops`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRequest {
    /// Name of the device: the namespace of `op_id`, and part of conflict copy names.
    pub device: String,
    pub op_id: u64,
    pub op: RemoteOp,
    /// The client's cursor and `cursor_tag`, checked before anything else (0/`None`: no check).
    #[serde(default)]
    pub cursor: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check: Option<String>,
}

/// Answer of `GET /api/sync/ops/{device}/{op_id}?with_op=true`. Without `with_op` the answer is
/// the bare [`RemoteResult`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpLookup {
    pub result: RemoteResult,
    /// `None` for operations the server stored before it kept them.
    pub op: Option<RemoteOp>,
}

/// Query of `GET /api/sync/ops/{device}/{op_id}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpResultQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub with_op: Option<String>,
}

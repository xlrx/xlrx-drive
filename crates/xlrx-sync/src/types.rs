//! Basic types of the engine: local IDs, fingerprints and the entries of the three trees.

use std::fmt;

use serde::{Deserialize, Serialize};
use xlrx_proto::{FileContent, Kind, Name, NodeId, Rev};

use crate::tree::TreeEntry;

/// Identity of an object in the local file system (e.g. inode or APFS file ID).
/// Stays the same across renames and moves.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalId(pub u64);

impl LocalId {
    /// Placeholder for "no local counterpart anymore" (e.g. when the inode number was reused for
    /// an object of a different kind). Never exists in the local tree.
    pub const GONE: LocalId = LocalId(u64::MAX);
}

impl fmt::Debug for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "l{}", self.0)
    }
}

/// Attributes of a local file that change with every modification (size, mtime, ctime).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// ID of an operation. For server operations it is also the idempotency key:
/// if the client retries the same operation after an interruption, the server runs it only once.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpId(pub u64);

impl fmt::Debug for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op{}", self.0)
    }
}

/// State of a node on the server (remote tree R).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub parent: NodeId,
    pub name: Name,
    pub kind: Kind,
    /// Files only.
    pub content: Option<FileContent>,
    pub rev: Rev,
}

impl TreeEntry<NodeId> for RemoteEntry {
    fn parent(&self) -> NodeId {
        self.parent
    }
    fn name(&self) -> &Name {
        &self.name
    }
}

/// Last agreed state of a node (synced tree S): the server and the local copy agreed on this.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SyncedEntry {
    pub parent: NodeId,
    pub name: Name,
    pub kind: Kind,
    pub content: Option<FileContent>,
    pub rev: Rev,
    /// Local counterpart.
    pub local: LocalId,
    /// Fingerprint of the local file when its content was last `content`.
    pub fp: Option<Fingerprint>,
}

/// Observed state of a local object (local tree L).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LocalEntry {
    pub parent: LocalId,
    pub name: Name,
    pub kind: Kind,
    pub fp: Option<Fingerprint>,
    /// Files only: content matching the fingerprint (hashed by the scanner or from the hash cache).
    pub content: Option<FileContent>,
}

impl TreeEntry<LocalId> for LocalEntry {
    fn parent(&self) -> LocalId {
        self.parent
    }
    fn name(&self) -> &Name {
        &self.name
    }
}

/// An observation from a scan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LocalObservation {
    pub id: LocalId,
    pub entry: LocalEntry,
}

/// A change from the server journal: current state or `None` (deleted or no longer visible).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RemoteChange {
    pub node: NodeId,
    pub state: Option<RemoteEntry>,
}

/// Configuration of a sync directory.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Server node corresponding to the local sync directory.
    pub remote_root: NodeId,
    /// Device name; appears in the names of conflict copies.
    pub device: String,
    /// Is the local file system case-insensitive (APFS default)?
    pub local_case_insensitive: bool,
    /// More deletions than this at once are only sent to the server after confirmation.
    pub max_unconfirmed_deletes: usize,
}

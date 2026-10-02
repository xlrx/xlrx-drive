//! Operations planned by the engine, and their results.
//!
//! The engine executes nothing itself. The caller (client or simulator) executes the operations
//! and reports the result back. Every operation carries the preconditions that must be checked
//! when executing it. If the state has changed in the meantime, the operation is not executed
//! (`Precondition` or `Rejected`), and the engine replans.

use serde::{Deserialize, Serialize};
use xlrx_proto::{FileContent, Name, NodeId, Rev, Seq};

use crate::types::{Fingerprint, LocalId, OpId};

/// Origin of a newly created server node.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Origin {
    /// Ordinary new local object; it is located at the same place locally as on the server.
    New,
    /// Conflict copy: the local object is still at `local_parent`/`local_name` and is renamed
    /// locally to the conflict name afterwards. If it was previously linked to `replaces`, that
    /// link is dissolved (node `replaces` is then downloaded again).
    ConflictCopy {
        local_parent: NodeId,
        local_name: Name,
        replaces: Option<NodeId>,
    },
}

/// Operation on the server. Persisted in the outbox before sending and retried with the same
/// [`OpId`] after a restart; the server executes each `OpId` only once.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RemoteOp {
    CreateDir {
        parent: NodeId,
        name: Name,
        source: LocalId,
        origin: Origin,
    },
    /// New file. The executor uploads the content from `source`, checking that the file still has
    /// fingerprint `fp` and content `content` (otherwise `SourceChanged`).
    CreateFile {
        parent: NodeId,
        name: Name,
        content: FileContent,
        source: LocalId,
        fp: Fingerprint,
        origin: Origin,
    },
    /// New content for an existing file. Only valid if the server still has `base_rev`;
    /// otherwise the server stores the content as a conflict copy (result `Conflict`).
    Upload {
        node: NodeId,
        base_rev: Rev,
        content: FileContent,
        source: LocalId,
        fp: Fingerprint,
    },
    /// Move/rename, but only if the node is still at `from_parent`/`from_name`.
    /// If someone else has moved it in the meantime, their move wins (`Rejected(Moved)`).
    Move {
        node: NodeId,
        from_parent: NodeId,
        from_name: Name,
        parent: NodeId,
        name: Name,
    },
    /// Deletes a file, but only if it still has `base_rev` and is at `parent`/`name`.
    /// That way, content the client does not know and moves it has not seen are never lost.
    DeleteFile {
        node: NodeId,
        base_rev: Rev,
        parent: NodeId,
        name: Name,
    },
    /// Deletes a directory, but only if it is empty on the server and is at `parent`/`name`.
    DeleteDir {
        node: NodeId,
        parent: NodeId,
        name: Name,
    },
}

/// Reason why the server rejected an operation. The state remains unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Reject {
    NameTaken,
    ParentGone,
    NodeGone,
    NotEmpty,
    RevMismatch,
    WouldCycle,
    /// The node is not (or no longer) at the expected location.
    Moved,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum RemoteResult {
    Created {
        node: NodeId,
        rev: Rev,
        seq: Seq,
    },
    Updated {
        rev: Rev,
        seq: Seq,
    },
    /// `Upload` with an outdated base: the server stored the uploaded content as a new node
    /// (conflict copy) next to the original. Nothing was overwritten.
    Conflict {
        node: NodeId,
        rev: Rev,
        seq: Seq,
    },
    Moved {
        seq: Seq,
    },
    Deleted {
        seq: Seq,
    },
    Rejected(Reject),
    /// The local source file changed before or during the upload. Nothing was applied.
    SourceChanged,
    /// Network error or similar: whether the operation was executed is unknown. Retry later with
    /// the same ID.
    Transient,
}

/// Expected state of a local file before an operation that replaces or deletes it.
///
/// The executor checks the fingerprint and – if the fingerprint lies in the unsafe time window
/// (coarse timestamps, just written) – additionally the content by rehashing it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Expected {
    pub fp: Fingerprint,
    pub content: FileContent,
}

/// Operation on the local file system.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LocalOp {
    /// Create a directory. Fails if the name is taken.
    CreateDir {
        parent: LocalId,
        name: Name,
        node: NodeId,
        node_parent: NodeId,
    },
    /// Download a new file: write it to a temporary file, verify the hash, then move it to the
    /// target name without replacing. Fails if the name is taken.
    Download {
        parent: LocalId,
        name: Name,
        node: NodeId,
        node_parent: NodeId,
        rev: Rev,
        content: FileContent,
    },
    /// Replace the content of an existing file, but only if it is still at `parent`/`name` and
    /// matches `expect`. The executor atomically swaps the finished new file with the original and
    /// then checks the swapped-out original again; if it has changed in the meantime, the swap is
    /// reverted (nothing is lost).
    Replace {
        local: LocalId,
        parent: LocalId,
        name: Name,
        expect: Expected,
        node: NodeId,
        rev: Rev,
        content: FileContent,
    },
    /// Rename/move without replacing. Checks that the object is still at `from_parent`/`from_name`.
    /// `synced_to`: the location that counts as agreed afterwards (`None` for purely local renames
    /// that only move an object out of the way).
    Move {
        local: LocalId,
        from_parent: LocalId,
        from_name: Name,
        parent: LocalId,
        name: Name,
        synced_to: Option<(NodeId, Name)>,
    },
    /// Delete a file, but only if it is still at `parent`/`name` and matches `expect`.
    /// To do so, the executor moves it into its own trash and checks it again there;
    /// if it has changed, it is moved back.
    DeleteFile {
        local: LocalId,
        parent: LocalId,
        name: Name,
        expect: Expected,
        node: NodeId,
    },
    /// Delete a directory, but only if it is empty and is at `parent`/`name`.
    DeleteDir {
        local: LocalId,
        parent: LocalId,
        name: Name,
        node: NodeId,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum LocalResult {
    /// Executed. `id`/`fp` describe the resulting object (possibly a new identity for `Replace`).
    Done {
        id: LocalId,
        fp: Option<Fingerprint>,
    },
    /// Precondition violated (changed, gone, name taken). Nothing was changed.
    Precondition,
    /// Other error. Nothing was changed.
    Error,
}

/// A planned operation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Op {
    Remote(OpId, RemoteOp),
    Local(OpId, LocalOp),
}

//! Sans-IO sync engine of xlrx-drive.
//!
//! The engine only makes decisions. The caller provides the file system, network and clock:
//!
//! 1. Inputs: [`Engine::on_remote_changes`] (server journal), [`Engine::on_local_snapshot`] (scan),
//!    results of executed operations ([`Engine::on_remote_result`], [`Engine::on_local_result`]).
//! 2. Output: [`Engine::plan`] returns operations with preconditions.
//! 3. After every input and after `plan`, [`Engine::state`] is persisted.
//!
//! As a result, the same engine runs in the Mac client, the iOS app and the deterministic simulator
//! (`xlrx-sim`), which runs millions of random scenarios with crashes and network failures.

mod engine;
mod ops;
mod synced;
mod tree;
mod types;

pub use engine::{DOWNLOAD_TEMP_PREFIX, Engine, State, TEMP_PREFIX};
pub use ops::{Expected, LocalOp, LocalResult, Op, Origin, Reject, RemoteOp, RemoteResult};
pub use synced::Synced;
pub use tree::{Tree, TreeEntry};
pub use types::{
    Config, Fingerprint, LocalEntry, LocalId, LocalObservation, OpId, RemoteChange, RemoteEntry,
    SyncedEntry,
};

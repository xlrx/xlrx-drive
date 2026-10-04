//! Shared types for the xlrx-drive server and clients.
//!
//! Everything that goes over the wire or is persisted on both sides lives here:
//! IDs, revisions, content hashes and file names.

mod content;
mod fold_table;
mod ids;
pub mod name;

pub use content::{ContentHash, FileContent};
pub use ids::{Kind, NodeId, Rev, Seq};
pub use name::{Name, NameError, NameRefused};

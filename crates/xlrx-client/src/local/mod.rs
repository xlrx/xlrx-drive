//! The local side of a sync folder (ADR 0002 §7).

pub mod id;
pub mod index;
pub mod scan;
pub mod store;

pub use id::local_id;
pub use index::LocalIndex;
pub use scan::{ScanAbort, Snapshot, full_scan};
pub use store::LocalStore;

//! The local side of a sync folder (ADR 0002 §7).

pub mod id;
pub mod store;

pub use id::local_id;
pub use store::LocalStore;

//! The xlrx-drive sync client (ADR 0002): everything that is not the engine's decision.
//!
//! - [`local`]: the local side — identities of files, the client's own state on disk (hash
//!   cache, intents before irreversible steps, its trash), scanning and executing operations.

pub mod local;

//! Deterministic simulator for the xlrx-drive sync engine.
//!
//! A run consists of a simulated server and several clients, each with its own simulated file
//! system. Random user actions (on the clients and directly on the server), sync steps in random
//! order, lost requests and responses, and crashes at arbitrary points alternate. This is followed
//! by a settle phase, after which the following is checked:
//!
//! - **Convergence:** All clients have exactly the server's state.
//! - **Data preservation:** Any content a user wrote and did not remove themselves still exists.
//! - **Invariants** of the engine after every step.
//!
//! Every failure is exactly reproducible from its seed.

mod driver;
pub mod fs;
mod rng;
pub mod scenario;
pub mod server;
mod sim;
mod trace;

pub use sim::{SimConfig, SimFailure, SimStats, run};

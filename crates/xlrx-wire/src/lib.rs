//! The JSON forms of the xlrx-drive server API as a client sends and receives them (ADR 0002).
//!
//! The server keeps its own types; the golden files in `tests/golden/` are written from the
//! server's types (`crates/xlrx-server/tests/wire_formen.rs`) and read back here, so the two can
//! never drift apart unnoticed. HTTP itself (paths, methods, status codes) is the client's
//! business; this crate holds only the shapes.

pub mod devices;
pub mod error;
pub mod roots;
pub mod sync;
pub mod uploads;

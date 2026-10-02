//! Sans-IO Sync-Engine von xlrx-drive.
//!
//! Die Engine trifft nur Entscheidungen. Dateisystem, Netzwerk und Uhr liefert der Aufrufer:
//!
//! 1. Eingaben: [`Engine::on_remote_changes`] (Server-Journal), [`Engine::on_local_snapshot`] (Scan),
//!    Ergebnisse ausgeführter Operationen ([`Engine::on_remote_result`], [`Engine::on_local_result`]).
//! 2. Ausgabe: [`Engine::plan`] liefert Operationen mit Vorbedingungen.
//! 3. Nach jeder Eingabe und nach `plan` wird [`Engine::state`] persistiert.
//!
//! Dadurch läuft dieselbe Engine im Mac-Client, in der iOS-App und im deterministischen Simulator
//! (`xlrx-sim`), der Millionen zufälliger Szenarien mit Abstürzen und Netzfehlern durchspielt.

mod engine;
mod ops;
mod synced;
mod tree;
mod types;

pub use engine::{Engine, State};
pub use ops::{LocalOp, LocalResult, Op, Origin, Reject, RemoteOp, RemoteResult};
pub use synced::Synced;
pub use tree::{Tree, TreeEntry};
pub use types::{
    Config, Fingerprint, LocalEntry, LocalId, LocalObservation, OpId, RemoteChange, RemoteEntry,
    SyncedEntry,
};

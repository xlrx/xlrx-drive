//! Deterministischer Simulator für die Sync-Engine von xlrx-drive.
//!
//! Ein Lauf besteht aus einem simulierten Server und mehreren Clients mit eigenem simuliertem
//! Dateisystem. Zufällige Nutzeraktionen (auf den Clients und direkt auf dem Server), Sync-Schritte
//! in zufälliger Reihenfolge, verlorene Anfragen und Antworten sowie Abstürze an beliebigen Stellen
//! wechseln sich ab. Danach folgt eine Ruhephase, und es wird geprüft:
//!
//! - **Konvergenz:** Alle Clients haben exakt den Stand des Servers.
//! - **Datenerhalt:** Jeder Inhalt, den ein Nutzer geschrieben und nicht selbst entfernt hat, existiert noch.
//! - **Invarianten** der Engine nach jedem Schritt.
//!
//! Jeder Fehler ist über den Seed exakt reproduzierbar.

mod driver;
pub mod fs;
mod rng;
pub mod scenario;
pub mod server;
mod sim;

pub use sim::{SimConfig, SimFailure, SimStats, run};

//! Gemeinsame Typen für Server und Clients von xlrx-drive.
//!
//! Alles, was über die Leitung geht oder in beiden Welten persistiert wird, lebt hier:
//! IDs, Revisionen, Inhalts-Hashes und Dateinamen.

mod content;
mod ids;
pub mod name;

pub use content::{ContentHash, FileContent};
pub use ids::{Kind, NodeId, Rev, Seq};
pub use name::{Name, NameError};

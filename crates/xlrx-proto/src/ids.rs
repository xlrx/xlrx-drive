use std::fmt;

use serde::{Deserialize, Serialize};

/// Stabile, vom Server vergebene ID eines Knotens (Datei oder Ordner).
///
/// Pfade sind nur abgeleitet. Umbenennen und Verschieben ändern die ID nie.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub u64);

impl fmt::Debug for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "n{}", self.0)
    }
}

/// Inhalts-Revision einer Datei. Ändert sich bei jedem neuen Inhalt, nicht bei Umbenennungen.
///
/// Dient als Vorbedingung für Uploads und Löschungen: Der Server führt sie nur aus, wenn der Client
/// die aktuelle Revision kennt. So wird nie Inhalt überschrieben oder gelöscht, den der Client nicht gesehen hat.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rev(pub u64);

impl fmt::Debug for Rev {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// Global monotone Sequenznummer des Server-Journals.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Seq(pub u64);

impl fmt::Debug for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "s{}", self.0)
    }
}

/// Art eines Knotens.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Dir,
    File,
}

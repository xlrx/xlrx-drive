//! Grundtypen der Engine: lokale IDs, Fingerprints und die Einträge der drei Bäume.

use std::fmt;

use serde::{Deserialize, Serialize};
use xlrx_proto::{FileContent, Kind, Name, NodeId, Rev};

use crate::tree::TreeEntry;

/// Identität eines Objekts im lokalen Dateisystem (z.B. Inode bzw. APFS-File-ID).
/// Bleibt bei Umbenennen und Verschieben gleich.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LocalId(pub u64);

impl LocalId {
    /// Platzhalter für „keine lokale Entsprechung mehr“ (z.B. wenn die Inode-Nummer für ein
    /// Objekt anderer Art wiederverwendet wurde). Existiert nie im lokalen Baum.
    pub const GONE: LocalId = LocalId(u64::MAX);
}

impl fmt::Debug for LocalId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "l{}", self.0)
    }
}

/// Merkmale einer lokalen Datei, die sich bei jeder Änderung ändern (Größe, mtime, ctime).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// ID einer Operation. Für Server-Operationen zugleich der Idempotenz-Schlüssel:
/// Wiederholt der Client nach einem Abbruch dieselbe Operation, führt der Server sie nur einmal aus.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct OpId(pub u64);

impl fmt::Debug for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "op{}", self.0)
    }
}

/// Zustand eines Knotens auf dem Server (Remote-Baum R).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RemoteEntry {
    pub parent: NodeId,
    pub name: Name,
    pub kind: Kind,
    /// Nur bei Dateien.
    pub content: Option<FileContent>,
    pub rev: Rev,
}

impl TreeEntry<NodeId> for RemoteEntry {
    fn parent(&self) -> NodeId {
        self.parent
    }
    fn name(&self) -> &Name {
        &self.name
    }
}

/// Zuletzt vereinbarter Zustand eines Knotens (Synced-Baum S): Server und lokale Kopie waren sich hier einig.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SyncedEntry {
    pub parent: NodeId,
    pub name: Name,
    pub kind: Kind,
    pub content: Option<FileContent>,
    pub rev: Rev,
    /// Lokale Entsprechung.
    pub local: LocalId,
    /// Fingerprint der lokalen Datei, als ihr Inhalt zuletzt `content` war.
    pub fp: Option<Fingerprint>,
}

/// Beobachteter Zustand eines lokalen Objekts (Lokaler Baum L).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct LocalEntry {
    pub parent: LocalId,
    pub name: Name,
    pub kind: Kind,
    pub fp: Option<Fingerprint>,
    /// Bei Dateien der Inhalt zum Fingerprint (vom Scanner gehasht oder aus dem Hash-Cache).
    pub content: Option<FileContent>,
}

impl TreeEntry<LocalId> for LocalEntry {
    fn parent(&self) -> LocalId {
        self.parent
    }
    fn name(&self) -> &Name {
        &self.name
    }
}

/// Eine Beobachtung aus einem Scan.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct LocalObservation {
    pub id: LocalId,
    pub entry: LocalEntry,
}

/// Eine Änderung aus dem Server-Journal: aktueller Zustand oder `None` (gelöscht bzw. nicht mehr sichtbar).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RemoteChange {
    pub node: NodeId,
    pub state: Option<RemoteEntry>,
}

/// Konfiguration eines Sync-Ordners.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    /// Server-Knoten, der dem lokalen Sync-Ordner entspricht.
    pub remote_root: NodeId,
    /// Gerätename, erscheint in Namen von Konfliktkopien.
    pub device: String,
    /// Ignoriert das lokale Dateisystem Groß-/Kleinschreibung (APFS-Standard)?
    pub local_case_insensitive: bool,
    /// Mehr Löschungen auf einmal werden erst nach Bestätigung an den Server geschickt.
    pub max_unconfirmed_deletes: usize,
}

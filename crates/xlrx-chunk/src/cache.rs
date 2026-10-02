//! Hash-Cache: Unveränderte Dateien werden nie erneut gelesen.
//!
//! Schlüssel ist die Datei-Identität, gültig ist ein Eintrag nur bei identischem Fingerprint.
//! Schutz vor dem „Racy-Timestamp“-Problem (wie bei git): Lag die letzte Änderung zu nah am Zeitpunkt
//! des Hashens, könnte eine weitere Änderung in derselben Zeitstempel-Granularität unbemerkt bleiben.
//! Solche Einträge gelten als unsicher und werden beim nächsten Mal neu gehasht.

use std::collections::HashMap;

use xlrx_proto::FileContent;

use crate::{FileId, Fingerprint};

/// Abstand, den mtime/ctime zum Hash-Zeitpunkt mindestens haben müssen (2 s, deckt auch
/// Dateisysteme mit grober Zeitauflösung wie FAT oder manche Netzwerk-Dateisysteme ab).
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CacheEntry {
    pub fingerprint: Fingerprint,
    /// Wanduhrzeit (ns seit Unix-Epoche), zu der gehasht wurde.
    pub hashed_at_ns: i64,
    pub content: FileContent,
}

impl CacheEntry {
    /// `true`, wenn die letzte Änderung so kurz vor dem Hashen lag, dass der Eintrag nicht vertrauenswürdig ist.
    pub fn is_racy(&self) -> bool {
        let newest = self.fingerprint.mtime_ns.max(self.fingerprint.ctime_ns);
        newest > self.hashed_at_ns.saturating_sub(RACY_WINDOW_NS)
    }
}

#[derive(Default, Debug)]
pub struct HashCache {
    map: HashMap<FileId, CacheEntry>,
}

impl HashCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Liefert den Inhalt nur, wenn der Fingerprint exakt passt und der Eintrag nicht „racy“ ist.
    pub fn lookup(&self, id: FileId, fingerprint: Fingerprint) -> Option<FileContent> {
        let e = self.map.get(&id)?;
        (e.fingerprint == fingerprint && !e.is_racy()).then_some(e.content)
    }

    pub fn insert(&mut self, id: FileId, entry: CacheEntry) {
        self.map.insert(id, entry);
    }

    pub fn remove(&mut self, id: FileId) {
        self.map.remove(&id);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xlrx_proto::ContentHash;

    fn entry(mtime: i64, hashed_at: i64) -> CacheEntry {
        CacheEntry {
            fingerprint: Fingerprint {
                size: 1,
                mtime_ns: mtime,
                ctime_ns: mtime,
            },
            hashed_at_ns: hashed_at,
            content: FileContent {
                hash: ContentHash([1; 32]),
                size: 1,
            },
        }
    }

    #[test]
    fn hit_only_with_identical_fingerprint() {
        let mut c = HashCache::new();
        let id = FileId { dev: 1, ino: 2 };
        let e = entry(0, 10 * RACY_WINDOW_NS);
        c.insert(id, e);
        assert_eq!(c.lookup(id, e.fingerprint), Some(e.content));
        let mut other = e.fingerprint;
        other.ctime_ns += 1;
        assert_eq!(c.lookup(id, other), None);
    }

    #[test]
    fn racy_entries_are_not_trusted() {
        let mut c = HashCache::new();
        let id = FileId { dev: 1, ino: 2 };
        let e = entry(1_000, 1_000 + RACY_WINDOW_NS / 2);
        assert!(e.is_racy());
        c.insert(id, e);
        assert_eq!(c.lookup(id, e.fingerprint), None);
    }
}

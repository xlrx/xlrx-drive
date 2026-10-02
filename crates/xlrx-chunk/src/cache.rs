//! Hash cache: unchanged files are never read again.
//!
//! The key is the file identity; an entry is only valid if the fingerprint is identical.
//! Protection against the "racy timestamp" problem (as in git): if the last modification was too
//! close to the time of hashing, a further modification within the same timestamp granularity
//! could go unnoticed. Such entries are considered unsafe and are rehashed the next time.

use std::collections::HashMap;

use xlrx_proto::FileContent;

use crate::{FileId, Fingerprint};

/// Minimum distance mtime/ctime must have from the time of hashing (2 s; also covers file
/// systems with coarse time resolution such as FAT or some network file systems).
pub const RACY_WINDOW_NS: i64 = 2_000_000_000;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CacheEntry {
    pub fingerprint: Fingerprint,
    /// Wall-clock time (ns since the Unix epoch) at which the file was hashed.
    pub hashed_at_ns: i64,
    pub content: FileContent,
}

impl CacheEntry {
    /// `true` if the last modification was so close to hashing that the entry cannot be trusted.
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

    /// Returns the content only if the fingerprint matches exactly and the entry is not "racy".
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

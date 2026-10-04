//! What the client knows about each local object beyond the engine's view: the raw name as it
//! lies on disk (the engine sees it in NFC) and its parent, to find the object again.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use xlrx_proto::Kind;
use xlrx_sync::LocalId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub parent: LocalId,
    /// The name exactly as on disk (maybe NFD).
    pub raw: OsString,
    pub kind: Kind,
}

#[derive(Clone, Debug, Default)]
pub struct LocalIndex {
    root: Option<LocalId>,
    entries: HashMap<LocalId, IndexEntry>,
}

impl LocalIndex {
    pub fn new(root: LocalId) -> Self {
        Self {
            root: Some(root),
            entries: HashMap::new(),
        }
    }

    pub fn root(&self) -> Option<LocalId> {
        self.root
    }

    pub fn insert(&mut self, id: LocalId, entry: IndexEntry) {
        self.entries.insert(id, entry);
    }

    pub fn remove(&mut self, id: LocalId) -> Option<IndexEntry> {
        self.entries.remove(&id)
    }

    pub fn get(&self, id: LocalId) -> Option<&IndexEntry> {
        self.entries.get(&id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (LocalId, &IndexEntry)> {
        self.entries.iter().map(|(id, e)| (*id, e))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The chain of `(id, raw name)` from the root down to `id` (empty for the root itself), or
    /// `None` if a link is missing or the chain does not end at the root (a cycle).
    pub fn chain(&self, id: LocalId) -> Option<Vec<(LocalId, &OsStr)>> {
        let root = self.root?;
        let mut out = Vec::new();
        let mut at = id;
        while at != root {
            if out.len() > self.entries.len() {
                return None;
            }
            let e = self.entries.get(&at)?;
            out.push((at, e.raw.as_os_str()));
            at = e.parent;
        }
        out.reverse();
        Some(out)
    }

    /// Path relative to the sync folder (for messages and the trash book).
    pub fn path(&self, id: LocalId) -> Option<PathBuf> {
        Some(self.chain(id)?.into_iter().map(|(_, n)| n).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pfade_und_ketten() {
        let mut ix = LocalIndex::new(LocalId(1));
        let dir = |p: u64, n: &str| IndexEntry {
            parent: LocalId(p),
            raw: n.into(),
            kind: Kind::Dir,
        };
        ix.insert(LocalId(2), dir(1, "A\u{0308}"));
        ix.insert(LocalId(3), dir(2, "b"));
        assert_eq!(ix.path(LocalId(3)), Some(PathBuf::from("A\u{0308}/b")));
        assert_eq!(ix.path(LocalId(1)), Some(PathBuf::new()));
        assert_eq!(ix.path(LocalId(9)), None);
        // A cycle never ends at the root.
        ix.insert(LocalId(4), dir(5, "x"));
        ix.insert(LocalId(5), dir(4, "y"));
        assert_eq!(ix.path(LocalId(4)), None);
    }
}

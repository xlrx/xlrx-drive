//! The synced tree S with a reverse index local ID → node and an index by parent node.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use xlrx_proto::NodeId;

use crate::types::{LocalId, SyncedEntry};

#[derive(Clone, Debug, Default)]
pub struct Synced {
    entries: BTreeMap<NodeId, SyncedEntry>,
    by_local: BTreeMap<LocalId, NodeId>,
    by_parent: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// Counts every change (to detect whether a planning pass had any effect).
    mutations: u64,
}

impl Synced {
    pub fn get(&self, n: NodeId) -> Option<&SyncedEntry> {
        self.entries.get(&n)
    }

    pub fn contains(&self, n: NodeId) -> bool {
        self.entries.contains_key(&n)
    }

    pub fn node_of(&self, l: LocalId) -> Option<NodeId> {
        self.by_local.get(&l).copied()
    }

    /// Nodes whose agreed parent node is `p`.
    pub fn children(&self, p: NodeId) -> Vec<NodeId> {
        self.by_parent
            .get(&p)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    pub fn ids(&self) -> Vec<NodeId> {
        self.entries.keys().copied().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (NodeId, &SyncedEntry)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn mutations(&self) -> u64 {
        self.mutations
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Inserts or replaces. Refuses if the local ID is already linked to another node, because a
    /// local file must never correspond to two server nodes.
    pub fn insert(&mut self, n: NodeId, e: SyncedEntry) -> bool {
        if e.local != LocalId::GONE
            && let Some(other) = self.by_local.get(&e.local)
            && *other != n
        {
            return false;
        }
        self.remove(n);
        if e.local != LocalId::GONE {
            self.by_local.insert(e.local, n);
        }
        self.by_parent.entry(e.parent).or_default().insert(n);
        self.entries.insert(n, e);
        self.mutations += 1;
        true
    }

    pub fn remove(&mut self, n: NodeId) -> Option<SyncedEntry> {
        let e = self.entries.remove(&n)?;
        if self.by_local.get(&e.local) == Some(&n) {
            self.by_local.remove(&e.local);
        }
        if let Some(set) = self.by_parent.get_mut(&e.parent) {
            set.remove(&n);
            if set.is_empty() {
                self.by_parent.remove(&e.parent);
            }
        }
        self.mutations += 1;
        Some(e)
    }

    /// Modifies an entry. If that would set the local ID to one already in use, nothing changes.
    pub fn update(&mut self, n: NodeId, f: impl FnOnce(&mut SyncedEntry)) -> bool {
        let Some(old) = self.entries.get(&n).cloned() else {
            return false;
        };
        let mut new = old.clone();
        f(&mut new);
        if new.local != LocalId::GONE && self.by_local.get(&new.local).is_some_and(|o| *o != n) {
            return false;
        }
        self.insert(n, new)
    }

    /// Same entries (ignoring the change counter)?
    pub fn same_entries(&self, other: &Synced) -> bool {
        self.entries == other.entries
    }

    /// Checks the consistency of the indexes.
    pub fn check(&self) -> Result<(), String> {
        for (n, e) in &self.entries {
            if e.local != LocalId::GONE && self.by_local.get(&e.local) != Some(n) {
                return Err(format!("S: {n:?} → {:?} fehlt im Rückwärtsindex", e.local));
            }
            if !self.by_parent.get(&e.parent).is_some_and(|s| s.contains(n)) {
                return Err(format!("S: {n:?} fehlt im Elternindex"));
            }
        }
        for (l, n) in &self.by_local {
            match self.entries.get(n) {
                Some(e) if e.local == *l => {}
                _ => return Err(format!("S: Rückwärtsindex {l:?} → {n:?} ist verwaist")),
            }
        }
        for (p, kids) in &self.by_parent {
            for n in kids {
                if self.entries.get(n).is_none_or(|e| e.parent != *p) {
                    return Err(format!("S: Elternindex {p:?} → {n:?} ist verwaist"));
                }
            }
        }
        Ok(())
    }
}

/// Only the entries are persisted; the indexes are rebuilt on load.
impl Serialize for Synced {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.entries.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Synced {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let entries = BTreeMap::<NodeId, SyncedEntry>::deserialize(d)?;
        let mut s = Synced::default();
        for (n, e) in entries {
            if !s.insert(n, e) {
                return Err(serde::de::Error::custom(
                    "lokale ID ist mehreren Knoten zugeordnet",
                ));
            }
        }
        Ok(s)
    }
}

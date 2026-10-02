//! Der Synced-Baum S mit Rückwärtsindex lokale ID → Knoten.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use xlrx_proto::NodeId;

use crate::types::{LocalId, SyncedEntry};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Synced {
    entries: BTreeMap<NodeId, SyncedEntry>,
    by_local: BTreeMap<LocalId, NodeId>,
    /// Zählt jede Änderung (zum Erkennen, ob ein Planungsdurchlauf etwas bewirkt hat).
    #[serde(skip)]
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

    /// Fügt ein oder ersetzt. Verweigert, wenn die lokale ID schon mit einem anderen Knoten verknüpft ist,
    /// denn eine lokale Datei darf nie zwei Server-Knoten entsprechen.
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
        self.entries.insert(n, e);
        self.mutations += 1;
        true
    }

    pub fn remove(&mut self, n: NodeId) -> Option<SyncedEntry> {
        let e = self.entries.remove(&n)?;
        if self.by_local.get(&e.local) == Some(&n) {
            self.by_local.remove(&e.local);
        }
        self.mutations += 1;
        Some(e)
    }

    /// Ändert einen Eintrag. Wird dabei die lokale ID auf eine bereits vergebene gesetzt, bleibt alles unverändert.
    pub fn update(&mut self, n: NodeId, f: impl FnOnce(&mut SyncedEntry)) -> bool {
        let Some(old) = self.entries.get(&n).cloned() else {
            return false;
        };
        let mut new = old.clone();
        f(&mut new);
        if self.insert(n, new) {
            true
        } else {
            self.entries.insert(n, old);
            false
        }
    }

    /// Gleiche Einträge (ohne Änderungszähler)?
    pub fn same_entries(&self, other: &Synced) -> bool {
        self.entries == other.entries
    }

    /// Prüft die Konsistenz des Rückwärtsindex.
    pub fn check(&self) -> Result<(), String> {
        for (n, e) in &self.entries {
            if e.local != LocalId::GONE && self.by_local.get(&e.local) != Some(n) {
                return Err(format!("S: {n:?} → {:?} fehlt im Rückwärtsindex", e.local));
            }
        }
        for (l, n) in &self.by_local {
            match self.entries.get(n) {
                Some(e) if e.local == *l => {}
                _ => return Err(format!("S: Rückwärtsindex {l:?} → {n:?} ist verwaist")),
            }
        }
        Ok(())
    }
}

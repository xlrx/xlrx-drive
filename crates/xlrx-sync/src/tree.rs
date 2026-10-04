//! Tree with parent, children and name indexes.

use std::collections::{BTreeMap, BTreeSet};

use xlrx_proto::Name;

/// An entry that has a parent node and a name.
pub trait TreeEntry<I> {
    fn parent(&self) -> I;
    fn name(&self) -> &Name;
}

/// How names are compared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fold {
    Exact,
    /// Lowercase + NFC ([`Name::fold_key`]): the server's comparison.
    Lower,
    /// Full Unicode case folding ([`Name::local_fold_key`]): a case-insensitive local volume.
    Full,
}

/// A tree of entries, indexed by parent node and by (parent, name key).
///
/// Depending on the file system, the name key is the exact name or a folded form.
/// Multiple entries with the same key are allowed (temporarily, e.g. case variants on the server);
/// [`Tree::lookup`] then returns all of them.
#[derive(Clone, Debug)]
pub struct Tree<I, E> {
    entries: BTreeMap<I, E>,
    children: BTreeMap<I, BTreeSet<I>>,
    by_name: BTreeMap<(I, String), BTreeSet<I>>,
    fold: Fold,
}

impl<I: Ord + Copy, E: TreeEntry<I>> Tree<I, E> {
    /// A tree compared exactly, or case-insensitively like the server (`fold`).
    pub fn new(fold: bool) -> Self {
        Self::with(if fold { Fold::Lower } else { Fold::Exact })
    }

    /// The local tree: on a case-insensitive volume, names collide under full Unicode case
    /// folding (ADR 0002, E8).
    pub fn new_local(case_insensitive: bool) -> Self {
        Self::with(if case_insensitive {
            Fold::Full
        } else {
            Fold::Exact
        })
    }

    fn with(fold: Fold) -> Self {
        Self {
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            by_name: BTreeMap::new(),
            fold,
        }
    }

    pub fn key(&self, name: &Name) -> String {
        match self.fold {
            Fold::Exact => name.as_str().to_owned(),
            Fold::Lower => name.fold_key(),
            Fold::Full => name.local_fold_key(),
        }
    }

    pub fn get(&self, id: I) -> Option<&E> {
        self.entries.get(&id)
    }

    pub fn contains(&self, id: I) -> bool {
        self.entries.contains_key(&id)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn ids(&self) -> Vec<I> {
        self.entries.keys().copied().collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (I, &E)> {
        self.entries.iter().map(|(k, v)| (*k, v))
    }

    pub fn insert(&mut self, id: I, entry: E) {
        self.remove(id);
        self.children.entry(entry.parent()).or_default().insert(id);
        let key = (entry.parent(), self.key(entry.name()));
        self.by_name.entry(key).or_default().insert(id);
        self.entries.insert(id, entry);
    }

    pub fn remove(&mut self, id: I) -> Option<E> {
        let entry = self.entries.remove(&id)?;
        let parent = entry.parent();
        if let Some(set) = self.children.get_mut(&parent) {
            set.remove(&id);
            if set.is_empty() {
                self.children.remove(&parent);
            }
        }
        let key = (parent, self.key(entry.name()));
        if let Some(set) = self.by_name.get_mut(&key) {
            set.remove(&id);
            if set.is_empty() {
                self.by_name.remove(&key);
            }
        }
        Some(entry)
    }

    /// Modifies an entry and keeps the indexes consistent.
    pub fn update(&mut self, id: I, f: impl FnOnce(&mut E)) -> bool {
        match self.remove(id) {
            Some(mut e) => {
                f(&mut e);
                self.insert(id, e);
                true
            }
            None => false,
        }
    }

    pub fn children(&self, id: I) -> impl Iterator<Item = I> + '_ {
        self.children.get(&id).into_iter().flatten().copied()
    }

    pub fn has_children(&self, id: I) -> bool {
        self.children.get(&id).is_some_and(|s| !s.is_empty())
    }

    /// All entries under `parent` with the same name key as `name`.
    pub fn lookup(&self, parent: I, name: &Name) -> impl Iterator<Item = I> + '_ {
        self.by_name
            .get(&(parent, self.key(name)))
            .into_iter()
            .flatten()
            .copied()
    }

    /// Is `id` equal to `ancestor` or below it? Terminates safely on cycles.
    pub fn is_within(&self, id: I, ancestor: I) -> bool {
        let mut cur = id;
        for _ in 0..=self.entries.len() {
            if cur == ancestor {
                return true;
            }
            match self.entries.get(&cur) {
                Some(e) => cur = e.parent(),
                None => return false,
            }
        }
        false
    }

    /// Depth below the root (children of the root have depth 1). `None` on a broken chain or cycle.
    pub fn depth(&self, id: I, root: I) -> Option<usize> {
        let mut cur = id;
        for d in 0..=self.entries.len() {
            if cur == root {
                return Some(d);
            }
            cur = self.entries.get(&cur)?.parent();
        }
        None
    }

    /// Removes all entries that are not (or no longer) reachable from `root`.
    pub fn retain_reachable(&mut self, root: I) -> Vec<I> {
        let mut reachable = BTreeSet::new();
        let mut stack = vec![root];
        while let Some(cur) = stack.pop() {
            for c in self.children(cur) {
                if reachable.insert(c) {
                    stack.push(c);
                }
            }
        }
        let dead: Vec<I> = self
            .entries
            .keys()
            .filter(|k| !reachable.contains(k))
            .copied()
            .collect();
        for d in &dead {
            self.remove(*d);
        }
        dead
    }

    /// All descendants of `id` (excluding `id` itself).
    pub fn descendants(&self, id: I) -> Vec<I> {
        let mut out = Vec::new();
        let mut stack = vec![id];
        while let Some(cur) = stack.pop() {
            for c in self.children(cur) {
                out.push(c);
                stack.push(c);
            }
        }
        out
    }
}

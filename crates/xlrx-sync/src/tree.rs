//! Baum mit Eltern-, Kinder- und Namensindex.

use std::collections::{BTreeMap, BTreeSet};

use xlrx_proto::Name;

/// Ein Eintrag, der einen Elternknoten und einen Namen hat.
pub trait TreeEntry<I> {
    fn parent(&self) -> I;
    fn name(&self) -> &Name;
}

/// Ein Baum aus Einträgen mit Index nach Elternknoten und (Eltern, Namensschlüssel).
///
/// Der Namensschlüssel ist je nach Dateisystem der exakte Name oder die kleingeschriebene Form.
/// Mehrere Einträge mit gleichem Schlüssel sind erlaubt (kurzfristig, z.B. Groß-/Kleinschreibungsvarianten
/// auf dem Server); [`Tree::lookup`] liefert dann alle.
#[derive(Clone, Debug)]
pub struct Tree<I, E> {
    entries: BTreeMap<I, E>,
    children: BTreeMap<I, BTreeSet<I>>,
    by_name: BTreeMap<(I, String), BTreeSet<I>>,
    fold: bool,
}

impl<I: Ord + Copy, E: TreeEntry<I>> Tree<I, E> {
    pub fn new(fold: bool) -> Self {
        Self {
            entries: BTreeMap::new(),
            children: BTreeMap::new(),
            by_name: BTreeMap::new(),
            fold,
        }
    }

    pub fn key(&self, name: &Name) -> String {
        if self.fold {
            name.fold_key()
        } else {
            name.as_str().to_owned()
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

    /// Ändert einen Eintrag und hält die Indizes konsistent.
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

    /// Alle Einträge unter `parent` mit gleichem Namensschlüssel wie `name`.
    pub fn lookup(&self, parent: I, name: &Name) -> impl Iterator<Item = I> + '_ {
        self.by_name
            .get(&(parent, self.key(name)))
            .into_iter()
            .flatten()
            .copied()
    }

    /// Liegt `id` gleich `ancestor` oder darunter? Bricht bei Zyklen sicher ab.
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

    /// Tiefe unterhalb der Wurzel (Wurzel-Kinder haben Tiefe 1). `None` bei abgerissener Kette oder Zyklus.
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

    /// Entfernt alle Einträge, die nicht (mehr) von `root` aus erreichbar sind.
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

    /// Alle Nachfahren von `id` (ohne `id` selbst).
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

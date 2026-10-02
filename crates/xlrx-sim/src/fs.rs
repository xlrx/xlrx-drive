//! Simuliertes lokales Dateisystem mit Inodes, POSIX-ähnlichem `rename` und optional
//! Groß-/Kleinschreibungs-unabhängigen Namen (wie APFS).

use std::collections::BTreeMap;

use xlrx_proto::{FileContent, Kind, Name};
use xlrx_sync::{Fingerprint, LocalEntry, LocalId, LocalObservation};

#[derive(Clone, Debug)]
pub struct Inode {
    pub parent: u64,
    pub name: Name,
    pub kind: Kind,
    pub content: Option<FileContent>,
    pub mtime: i64,
    pub ctime: i64,
}

#[derive(Clone, Debug)]
pub struct SimFs {
    pub inodes: BTreeMap<u64, Inode>,
    pub root: u64,
    next_ino: u64,
    pub case_insensitive: bool,
    /// Auflösung der Zeitstempel (1 = exakt). Grobe Zeitstempel (z.B. exFAT: 2 s) lassen zwei
    /// Änderungen kurz hintereinander denselben Fingerprint haben.
    pub granularity: i64,
    /// Hash-Cache des Clients: Inode → (Fingerprint, Zeitpunkt des Hashens, Inhalt).
    cache: BTreeMap<u64, (Fingerprint, i64, FileContent)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FsError {
    NotFound,
    Exists,
    NotEmpty,
    NotADir,
    IsADir,
    Invalid,
}

impl SimFs {
    pub fn new(case_insensitive: bool, first_ino: u64) -> Self {
        Self {
            inodes: BTreeMap::new(),
            root: first_ino,
            next_ino: first_ino + 1,
            case_insensitive,
            granularity: 1,
            cache: BTreeMap::new(),
        }
    }

    fn key(&self, n: &Name) -> String {
        if self.case_insensitive {
            n.fold_key()
        } else {
            n.as_str().to_owned()
        }
    }

    pub fn is_dir(&self, ino: u64) -> bool {
        ino == self.root || self.inodes.get(&ino).is_some_and(|i| i.kind == Kind::Dir)
    }

    pub fn children(&self, dir: u64) -> Vec<u64> {
        self.inodes
            .iter()
            .filter(|(_, i)| i.parent == dir)
            .map(|(k, _)| *k)
            .collect()
    }

    pub fn lookup(&self, dir: u64, name: &Name) -> Option<u64> {
        let k = self.key(name);
        self.inodes
            .iter()
            .find(|(_, i)| i.parent == dir && self.key(&i.name) == k)
            .map(|(id, _)| *id)
    }

    /// Liegt `ino` gleich `anc` oder darunter?
    pub fn within(&self, ino: u64, anc: u64) -> bool {
        let mut cur = ino;
        loop {
            if cur == anc {
                return true;
            }
            if cur == self.root {
                return false;
            }
            match self.inodes.get(&cur) {
                Some(i) => cur = i.parent,
                None => return false,
            }
        }
    }

    pub fn fingerprint(&self, ino: u64) -> Option<Fingerprint> {
        let i = self.inodes.get(&ino)?;
        let c = i.content?;
        let g = self.granularity.max(1);
        Some(Fingerprint {
            size: c.size,
            mtime_ns: i.mtime.div_euclid(g) * g,
            ctime_ns: i.ctime.div_euclid(g) * g,
        })
    }

    /// Inhalt einer Datei, wie ihn der Client sieht: aus dem Hash-Cache, wenn der Fingerprint
    /// unverändert ist und der Hash erst nach dem Zeitstempel-Intervall der letzten Änderung
    /// entstand (sonst könnte eine spätere Änderung im selben Intervall unsichtbar sein);
    /// andernfalls wird neu „gehasht“. So arbeiten Scanner und Ausführender im echten Client.
    pub fn hashed_content(&mut self, ino: u64, now: i64) -> Option<FileContent> {
        let fp = self.fingerprint(ino)?;
        let g = self.granularity.max(1);
        if let Some((cfp, at, c)) = self.cache.get(&ino)
            && *cfp == fp
            && *at >= fp.mtime_ns.max(fp.ctime_ns) + g
        {
            return Some(*c);
        }
        let c = self.inodes.get(&ino)?.content?;
        self.cache.insert(ino, (fp, now, c));
        Some(c)
    }

    /// Scan, wie ihn der Client liefert: Inhalte über den Hash-Cache.
    pub fn scan(&mut self, now: i64) -> Vec<LocalObservation> {
        let ids: Vec<u64> = self.inodes.keys().copied().collect();
        self.cache.retain(|k, _| ids.binary_search(k).is_ok());
        ids.into_iter()
            .map(|id| {
                let content = self.hashed_content(id, now);
                let i = &self.inodes[&id];
                LocalObservation {
                    id: LocalId(id),
                    entry: LocalEntry {
                        parent: LocalId(i.parent),
                        name: i.name.clone(),
                        kind: i.kind,
                        fp: self.fingerprint(id),
                        content,
                    },
                }
            })
            .collect()
    }

    pub fn create(
        &mut self,
        dir: u64,
        name: &Name,
        kind: Kind,
        content: Option<FileContent>,
        clock: i64,
    ) -> Result<u64, FsError> {
        if !self.is_dir(dir) {
            return Err(FsError::NotADir);
        }
        if self.lookup(dir, name).is_some() {
            return Err(FsError::Exists);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inodes.insert(
            ino,
            Inode {
                parent: dir,
                name: name.clone(),
                kind,
                content,
                mtime: clock,
                ctime: clock,
            },
        );
        Ok(ino)
    }

    pub fn write(&mut self, ino: u64, content: FileContent, clock: i64) -> Result<(), FsError> {
        let i = self.inodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        if i.kind != Kind::File {
            return Err(FsError::IsADir);
        }
        i.content = Some(content);
        i.mtime = clock;
        i.ctime = clock;
        Ok(())
    }

    /// Umbenennen. Mit `replace` wird eine vorhandene Zieldatei ersetzt (POSIX-Semantik);
    /// zurückgegeben wird dann deren Inode (gelöscht).
    pub fn rename(
        &mut self,
        ino: u64,
        dir: u64,
        name: &Name,
        replace: bool,
        clock: i64,
    ) -> Result<Option<Inode>, FsError> {
        if ino == self.root || !self.inodes.contains_key(&ino) {
            return Err(FsError::NotFound);
        }
        if !self.is_dir(dir) {
            return Err(FsError::NotADir);
        }
        if self.within(dir, ino) {
            return Err(FsError::Invalid);
        }
        let mut removed = None;
        if let Some(t) = self.lookup(dir, name)
            && t != ino
        {
            let src_kind = self.inodes[&ino].kind;
            let t_kind = self.inodes[&t].kind;
            if !replace || src_kind != Kind::File || t_kind != Kind::File {
                return Err(FsError::Exists);
            }
            removed = self.inodes.remove(&t);
        }
        let i = self.inodes.get_mut(&ino).ok_or(FsError::NotFound)?;
        i.parent = dir;
        i.name = name.clone();
        i.ctime = clock;
        Ok(removed)
    }

    pub fn unlink(&mut self, ino: u64) -> Result<Inode, FsError> {
        match self.inodes.get(&ino) {
            None => Err(FsError::NotFound),
            Some(i) if i.kind == Kind::Dir => Err(FsError::IsADir),
            Some(_) => self.inodes.remove(&ino).ok_or(FsError::NotFound),
        }
    }

    pub fn rmdir(&mut self, ino: u64) -> Result<(), FsError> {
        match self.inodes.get(&ino) {
            None => Err(FsError::NotFound),
            Some(i) if i.kind != Kind::Dir => Err(FsError::NotADir),
            Some(_) if self.inodes.values().any(|c| c.parent == ino) => Err(FsError::NotEmpty),
            Some(_) => {
                self.inodes.remove(&ino);
                Ok(())
            }
        }
    }

    /// Löscht rekursiv und liefert alle entfernten Dateien.
    pub fn rm_rf(&mut self, ino: u64) -> Vec<Inode> {
        let mut out = Vec::new();
        for c in self.children(ino) {
            out.extend(self.rm_rf(c));
        }
        if let Some(i) = self.inodes.remove(&ino) {
            out.push(i);
        }
        out
    }

    /// Tatsächlicher Stand (Inhalt immer exakt, ohne Hash-Cache).
    pub fn snapshot(&self) -> Vec<LocalObservation> {
        self.inodes
            .iter()
            .map(|(id, i)| LocalObservation {
                id: LocalId(*id),
                entry: LocalEntry {
                    parent: LocalId(i.parent),
                    name: i.name.clone(),
                    kind: i.kind,
                    fp: self.fingerprint(*id),
                    content: i.content,
                },
            })
            .collect()
    }

    /// Pfad → (Art, Inhalt), für den Endvergleich.
    pub fn listing(&self) -> BTreeMap<String, (Kind, Option<FileContent>)> {
        let mut out = BTreeMap::new();
        for (id, i) in &self.inodes {
            if let Some(p) = self.path(*id) {
                out.insert(p, (i.kind, i.content));
            }
        }
        out
    }

    pub fn path(&self, ino: u64) -> Option<String> {
        let mut parts = Vec::new();
        let mut cur = ino;
        while cur != self.root {
            let i = self.inodes.get(&cur)?;
            parts.push(i.name.as_str().to_owned());
            cur = i.parent;
        }
        parts.reverse();
        Some(format!("/{}", parts.join("/")))
    }
}

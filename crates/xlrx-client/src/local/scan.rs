//! The full scan of a sync folder (ADR 0002 §7.3, §7.4): every object below the root, without
//! following links, with content from the hash cache or freshly hashed.
//!
//! Guarantees:
//! - **Nothing that is or could be linked is left out.** Omitted are only the client's own
//!   directory at the root and unlinked junk *files* (names from `ignored()`, except download
//!   temporaries, which the engine knows). Everything the engine cannot sync — links, special
//!   files, hard-linked files, mount points, unreadable entries, names the server cannot hold, NFC
//!   twins, duplicate identities, ignored directories — is reported as *opaque*, and directories
//!   among them are still walked, their descendants opaque too, so an identity moved into one is
//!   still seen. Directories that cannot be walked are listed as *blind*.
//! - **Never a partial snapshot.** Any other error reading a directory aborts the scan.
//! - **Moves during the scan are caught.** A scan is not atomic. Every directory is stamped when
//!   read; afterwards stamps are compared and changed directories read again (new directories
//!   recursively), up to three passes. A snapshot whose last pass changed nothing is `stable`.
//!
//! Until the engine knows opaque objects (E6), opaque objects are only reported, not observed:
//! the caller pauses the folder instead of syncing it ([`Snapshot::opaque`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};

use xlrx_chunk::Chunker;
use xlrx_fs::{Dir, FileKind, Meta};
use xlrx_proto::name::{CLIENT_DIR, DOWNLOAD_TEMP_PREFIX, ignored, syncable};
use xlrx_proto::{FileContent, Kind, Name};
use xlrx_sync::{Fingerprint, LocalEntry, LocalId, LocalObservation};

use super::id::local_id;
use super::index::{IndexEntry, LocalIndex};
use super::store::LocalStore;

/// `SF_DATALESS` (Apple `st_flags`): an iCloud placeholder whose content is not on disk.
const SF_DATALESS: u32 = 0x4000_0000;

/// How long a file must have been left alone before it is hashed (ADR 0002 §7.4).
#[derive(Clone, Copy, Debug)]
pub struct ScanSettings {
    /// Quiet period after a change (3 s).
    pub quiet_ns: i64,
    /// Quiet period for database files, which programs write in bursts (30 s).
    pub quiet_db_ns: i64,
    /// Passes comparing directory stamps after the first read (3).
    pub passes: u32,
}

impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            quiet_ns: 3_000_000_000,
            quiet_db_ns: 30_000_000_000,
            passes: 3,
        }
    }
}

/// Wall-clock time in nanoseconds since the Unix epoch.
pub trait Clock {
    fn now_ns(&self) -> i64;
    /// Waits until `now_ns() >= at` (only ever a few milliseconds).
    fn sleep_until(&self, at: i64);
}

/// The system clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
    }

    fn sleep_until(&self, at: i64) {
        let wait = at.saturating_sub(self.now_ns());
        if wait > 0 {
            std::thread::sleep(std::time::Duration::from_nanos(wait.unsigned_abs()));
        }
    }
}

/// Test hook: called after each directory was read (moves during a scan).
pub trait ScanHooks {
    fn dir_read(&self, rel: &Path);
}

/// Why an object is opaque.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpaqueWhy {
    Symlink,
    /// FIFO, socket, device.
    Special,
    /// A file with more than one hard link.
    HardLinked,
    /// A directory on another device.
    MountPoint,
    /// Not readable (`EACCES`/`EPERM`).
    NoAccess,
    /// Apple: content not on disk (`SF_DATALESS`).
    Dataless,
    /// Not UTF-8, or longer than 255 bytes in NFC.
    NameNotSyncable,
    /// Two raw names with the same NFC form in one directory.
    NfcTwin,
    /// Two objects with the same identity.
    DuplicateId,
    /// A directory with a name from `ignored()`, or a linked file renamed to one.
    IgnoredName,
    /// Below an opaque directory.
    InsideOpaque,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opaque {
    pub id: LocalId,
    pub path: PathBuf,
    pub kind: Option<Kind>,
    pub why: OpaqueWhy,
}

/// Result of a full scan.
#[derive(Debug)]
pub struct Snapshot {
    pub root: LocalId,
    /// Everything syncable, for `Engine::on_local_snapshot`.
    pub obs: Vec<LocalObservation>,
    /// Opaque objects (not in `obs` until the engine knows them, E6).
    pub opaque: Vec<Opaque>,
    /// Directories that could not be walked: while any exist, no server deletion may be released.
    pub blind: Vec<LocalId>,
    /// The last stamp comparison found no change.
    pub stable: bool,
    /// Raw names and parents of everything seen (also opaque objects).
    pub index: LocalIndex,
    /// Earliest end of a quiet period: some file was reported without content and needs a
    /// rescan then.
    pub retry_at_ns: Option<i64>,
}

#[derive(Debug)]
pub enum ScanAbort {
    /// The root itself cannot be opened or read.
    Root(io::Error),
    /// Reading below the root failed: nothing is delivered.
    Io { path: PathBuf, error: io::Error },
    /// The hash cache failed.
    Store(rusqlite::Error),
}

impl std::fmt::Display for ScanAbort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Root(e) => write!(f, "Ordner nicht lesbar: {e}"),
            Self::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Self::Store(e) => write!(f, "Hash-Cache: {e}"),
        }
    }
}

impl std::error::Error for ScanAbort {}

/// `EACCES` or `EPERM`.
fn denied(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::PermissionDenied
}

fn not_found(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound
}

/// What the scanner found for one object.
#[derive(Clone, Debug)]
struct Item {
    parent: LocalId,
    raw: OsString,
    meta: Meta,
    kind: Kind,
    opaque: Option<OpaqueWhy>,
    content: Option<FileContent>,
}

/// A directory as read: its stamp, when it was read, and whether everything below is opaque.
#[derive(Clone, Copy, Debug)]
struct Stamp {
    ino: u64,
    mtime_ns: i64,
    ctime_ns: i64,
    read_at_ns: i64,
    inside_opaque: bool,
}

/// File system timestamps come from a coarse clock (a few ms on Linux): a directory changed
/// this shortly before it was read may change again without a different stamp.
const STAMP_RESOLUTION_NS: i64 = 50_000_000;

impl Stamp {
    fn of(m: &Meta, read_at_ns: i64, inside_opaque: bool) -> Self {
        Self {
            ino: m.ino,
            mtime_ns: m.mtime_ns,
            ctime_ns: m.ctime_ns,
            read_at_ns,
            inside_opaque,
        }
    }

    /// Unchanged since it was read, and not changed so shortly before that a later change could
    /// hide behind the same timestamp. (A timestamp in the future cannot hide one: a change sets
    /// the current time.)
    fn settled(&self, m: &Meta) -> bool {
        self.ino == m.ino
            && self.mtime_ns == m.mtime_ns
            && self.ctime_ns == m.ctime_ns
            && !self.recent()
    }

    /// Changed within the timestamp resolution of the time it was read.
    fn recent(&self) -> bool {
        let t = self.mtime_ns.max(self.ctime_ns);
        t >= self.read_at_ns - STAMP_RESOLUTION_NS && t <= self.read_at_ns + STAMP_RESOLUTION_NS
    }

    /// Forces another read in the next pass.
    fn dirty(self) -> Self {
        Self {
            mtime_ns: i64::MIN,
            ..self
        }
    }
}

/// What hashing a file gave.
enum Hashed {
    Known(FileContent),
    /// Quiet period, changed while read, or gone: the engine waits for a later scan.
    Unknown,
    /// Not readable: opaque.
    Denied,
}

/// Is this a database file that programs write in bursts?
fn database_like(name: &OsStr) -> bool {
    let n = name.to_string_lossy().to_lowercase();
    [".sqlite", ".db", ".lrcat", "-wal", "-journal", "-shm"]
        .iter()
        .any(|s| n.ends_with(s))
}

struct Walk<'a> {
    root_dir: Dir,
    root: LocalId,
    store: &'a LocalStore,
    linked: &'a HashSet<LocalId>,
    settings: ScanSettings,
    clock: &'a dyn Clock,
    hooks: Option<&'a dyn ScanHooks>,
    chunker: Chunker,
    items: HashMap<LocalId, Item>,
    /// Children per parent, kept in step with `items`.
    children: HashMap<LocalId, HashSet<LocalId>>,
    /// Identities seen more than once.
    duplicates: BTreeSet<LocalId>,
    dirs: BTreeMap<LocalId, Stamp>,
    blind: BTreeSet<LocalId>,
    retry_at_ns: Option<i64>,
}

/// Scans the sync folder at `root_path`. `linked` are the local identities linked in the engine's
/// synced tree S (a linked file with an ignored name is reported, not left out).
pub fn full_scan(
    root_path: &Path,
    store: &LocalStore,
    linked: &HashSet<LocalId>,
    settings: ScanSettings,
    clock: &dyn Clock,
    hooks: Option<&dyn ScanHooks>,
) -> Result<Snapshot, ScanAbort> {
    let root_dir = Dir::open(root_path).map_err(ScanAbort::Root)?;
    let root_meta = root_dir.meta().map_err(ScanAbort::Root)?;
    let root = local_id(&root_meta);
    let mut w = Walk {
        root_dir,
        root,
        store,
        linked,
        settings,
        clock,
        hooks,
        chunker: Chunker::new(),
        items: HashMap::new(),
        children: HashMap::new(),
        duplicates: BTreeSet::new(),
        dirs: BTreeMap::new(),
        blind: BTreeSet::new(),
        retry_at_ns: None,
    };
    let dir = w
        .root_dir
        .open_dir(OsStr::new("."))
        .map_err(ScanAbort::Root)?;
    w.read_dir(&dir, root, &root_meta, false, true)?;
    let mut stable = false;
    for _ in 0..w.settings.passes {
        if !w.recheck()? {
            stable = true;
            break;
        }
    }
    Ok(w.finish(stable))
}

impl Walk<'_> {
    fn rel(&self, id: LocalId) -> PathBuf {
        let mut parts = Vec::new();
        let mut at = id;
        while at != self.root {
            let Some(i) = self.items.get(&at) else { break };
            parts.push(i.raw.clone());
            at = i.parent;
            if parts.len() > self.items.len() {
                break;
            }
        }
        parts.iter().rev().collect()
    }

    fn io(&self, at: LocalId, name: Option<&OsStr>, error: io::Error) -> ScanAbort {
        let mut path = self.rel(at);
        if let Some(n) = name {
            path.push(n);
        }
        ScanAbort::Io { path, error }
    }

    /// Reads one directory (and, recursively, its subdirectories).
    fn read_dir(
        &mut self,
        dir: &Dir,
        dir_id: LocalId,
        dir_meta: &Meta,
        inside_opaque: bool,
        is_root: bool,
    ) -> Result<(), ScanAbort> {
        let stamp = Stamp::of(dir_meta, self.clock.now_ns(), inside_opaque);
        self.dirs.insert(dir_id, stamp);
        let names = dir.entries().map_err(|e| self.io(dir_id, None, e))?;
        // NFC twins: two raw names with the same NFC form.
        let mut nfc_count: HashMap<String, u32> = HashMap::new();
        for (n, _) in &names {
            if let Some(s) = n.to_str()
                && let Ok(name) = Name::new(s)
            {
                *nfc_count.entry(name.as_str().to_owned()).or_default() += 1;
            }
        }
        let mut subdirs = Vec::new();
        for (raw, _) in names {
            if is_root && raw == CLIENT_DIR {
                continue;
            }
            let meta = match dir.stat(&raw) {
                Ok(m) => m,
                // Gone since the listing: fine, a later stamp comparison sees the change.
                Err(e) if not_found(&e) => continue,
                Err(e) if denied(&e) => {
                    // Entries cannot be examined: the directory is not walkable.
                    if is_root {
                        return Err(ScanAbort::Root(e));
                    }
                    self.forget_children(dir_id);
                    self.blind.insert(dir_id);
                    if let Some(i) = self.items.get_mut(&dir_id) {
                        i.opaque.get_or_insert(OpaqueWhy::NoAccess);
                    }
                    return Ok(());
                }
                Err(e) => return Err(self.io(dir_id, Some(&raw), e)),
            };
            let id = local_id(&meta);
            let kind = if meta.kind == FileKind::Dir {
                Kind::Dir
            } else {
                Kind::File
            };
            let name_str = raw.to_str();
            let ignored_name = name_str.is_some_and(ignored);
            let mut why = match meta.kind {
                FileKind::Symlink => Some(OpaqueWhy::Symlink),
                FileKind::Other => Some(OpaqueWhy::Special),
                FileKind::File if meta.nlink > 1 => Some(OpaqueWhy::HardLinked),
                FileKind::Dir if meta.dev != dir_meta.dev => Some(OpaqueWhy::MountPoint),
                _ => None,
            };
            if why.is_none() && meta.flags & SF_DATALESS != 0 {
                why = Some(OpaqueWhy::Dataless);
            }
            if why.is_none() && !syncable(&raw) {
                why = Some(OpaqueWhy::NameNotSyncable);
            }
            if why.is_none()
                && let Some(s) = name_str
                && let Ok(n) = Name::new(s)
                && nfc_count.get(n.as_str()).copied().unwrap_or(0) > 1
            {
                why = Some(OpaqueWhy::NfcTwin);
            }
            if why.is_none() && ignored_name {
                let download_temp = name_str.is_some_and(|s| s.starts_with(DOWNLOAD_TEMP_PREFIX));
                match meta.kind {
                    FileKind::Dir => why = Some(OpaqueWhy::IgnoredName),
                    _ if download_temp => {}
                    _ if self.linked.contains(&id) => why = Some(OpaqueWhy::IgnoredName),
                    // Unlinked junk file: the only thing ever left out.
                    _ => continue,
                }
            }
            if why.is_none() && inside_opaque {
                why = Some(OpaqueWhy::InsideOpaque);
            }
            let mut seen_twice = false;
            if let Some(old) = self.items.get(&id).cloned()
                && (old.parent != dir_id || old.raw != raw)
            {
                if self.still_at(id, old.parent, &old.raw) {
                    // Really in two places at once: hard links, a bind mount, an identity clash.
                    seen_twice = true;
                    self.duplicates.insert(id);
                } else {
                    // Moved during the scan: the new place counts. What was recorded below the
                    // old place is read again under the new one; the old parent is read again.
                    self.forget_children(id);
                    if let Some(st) = self.dirs.get(&old.parent).copied() {
                        self.dirs.insert(old.parent, st.dirty());
                    }
                }
            }
            // A directory seen twice is never walked twice (a bind mount inside itself would
            // never end).
            let walk = meta.kind == FileKind::Dir
                && !seen_twice
                && !matches!(
                    why,
                    Some(OpaqueWhy::MountPoint | OpaqueWhy::Dataless | OpaqueWhy::Symlink)
                );
            if meta.kind == FileKind::Dir && why == Some(OpaqueWhy::Dataless) {
                self.blind.insert(id);
            }
            let mut content = None;
            if meta.kind == FileKind::File && why.is_none() {
                match self.content(dir, &raw, id, &meta)? {
                    Hashed::Known(c) => content = Some(c),
                    Hashed::Unknown => {}
                    Hashed::Denied => why = Some(OpaqueWhy::NoAccess),
                }
            }
            self.put(
                id,
                Item {
                    parent: dir_id,
                    raw: raw.clone(),
                    meta,
                    kind,
                    opaque: why,
                    content,
                },
            );
            if walk {
                subdirs.push((raw, id, meta, why.is_some() || inside_opaque));
            }
        }
        if let Some(h) = self.hooks {
            h.dir_read(&self.rel(dir_id));
        }
        for (raw, id, meta, opaque_below) in subdirs {
            match dir.open_dir(&raw) {
                Ok(sub) => {
                    // The directory under that name may have changed since `stat`.
                    let now = sub.meta().map_err(|e| self.io(dir_id, Some(&raw), e))?;
                    if now.ino != meta.ino {
                        self.dirs.insert(dir_id, stamp.dirty());
                        continue;
                    }
                    self.read_dir(&sub, id, &now, opaque_below, false)?;
                }
                Err(e) if not_found(&e) => {
                    // Replaced or gone since `stat`: read this directory again.
                    self.dirs.insert(dir_id, stamp.dirty());
                }
                Err(e) if denied(&e) => {
                    if let Some(i) = self.items.get_mut(&id) {
                        i.opaque.get_or_insert(OpaqueWhy::NoAccess);
                    }
                    self.blind.insert(id);
                }
                Err(e) => return Err(self.io(dir_id, Some(&raw), e)),
            }
        }
        Ok(())
    }

    /// Content of a file: unknown during its quiet period, else from the hash cache, else
    /// hashed (and unknown if it changed while being read).
    fn content(
        &mut self,
        dir: &Dir,
        raw: &OsStr,
        id: LocalId,
        meta: &Meta,
    ) -> Result<Hashed, ScanAbort> {
        let fp = Fingerprint {
            size: meta.size,
            mtime_ns: meta.mtime_ns,
            ctime_ns: meta.ctime_ns,
        };
        let now = self.clock.now_ns();
        let quiet = if database_like(raw) {
            self.settings.quiet_db_ns
        } else {
            self.settings.quiet_ns
        };
        let newest = fp.mtime_ns.max(fp.ctime_ns);
        if newest > now.saturating_sub(quiet) {
            self.retry_at(newest.saturating_add(quiet));
            return Ok(Hashed::Unknown);
        }
        if let Some(c) = self.store.cached(id, fp).map_err(ScanAbort::Store)? {
            return Ok(Hashed::Known(c));
        }
        let file = match dir.open_file(raw) {
            Ok(f) => f,
            Err(e) if not_found(&e) => return Ok(Hashed::Unknown),
            Err(e) if denied(&e) => return Ok(Hashed::Denied),
            Err(e) => {
                return Err(ScanAbort::Io {
                    path: self.rel(id),
                    error: e,
                });
            }
        };
        let before = xlrx_fs::meta_of(&file).map_err(|e| ScanAbort::Io {
            path: self.rel(id),
            error: e,
        })?;
        if before.kind != FileKind::File || local_id(&before) != id {
            return Ok(Hashed::Unknown);
        }
        let hashed_at = self.clock.now_ns();
        let digest = self
            .chunker
            .digest_reader(&file)
            .map_err(|e| ScanAbort::Io {
                path: self.rel(id),
                error: e,
            })?;
        let after = xlrx_fs::meta_of(&file).map_err(|e| ScanAbort::Io {
            path: self.rel(id),
            error: e,
        })?;
        let unchanged = (before.size, before.mtime_ns, before.ctime_ns)
            == (after.size, after.mtime_ns, after.ctime_ns)
            && (after.size, after.mtime_ns, after.ctime_ns) == (fp.size, fp.mtime_ns, fp.ctime_ns)
            && digest.content.size == fp.size;
        if !unchanged {
            self.retry_at(now.saturating_add(self.settings.quiet_ns));
            return Ok(Hashed::Unknown);
        }
        self.store
            .remember(id, fp, hashed_at, digest.content)
            .map_err(ScanAbort::Store)?;
        Ok(Hashed::Known(digest.content))
    }

    fn retry_at(&mut self, at: i64) {
        self.retry_at_ns = Some(self.retry_at_ns.map_or(at, |r| r.min(at)));
    }

    /// Records an object (replacing an earlier record of the same identity).
    fn put(&mut self, id: LocalId, item: Item) {
        let parent = item.parent;
        if let Some(old) = self.items.insert(id, item)
            && old.parent != parent
            && let Some(set) = self.children.get_mut(&old.parent)
        {
            set.remove(&id);
        }
        self.children.entry(parent).or_default().insert(id);
    }

    /// Removes everything below `dir` from the results (it is read again, or not walkable).
    fn forget_children(&mut self, dir: LocalId) {
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for c in self.children.remove(&d).unwrap_or_default() {
                // Only if it still lies here (a moved object has a new parent already).
                if self.items.get(&c).is_some_and(|i| i.parent == d) {
                    self.items.remove(&c);
                    self.dirs.remove(&c);
                    stack.push(c);
                }
            }
        }
    }

    /// Does `parent` (as recorded) still hold `id` under `raw`?
    fn still_at(&self, id: LocalId, parent: LocalId, raw: &OsStr) -> bool {
        let dir = if parent == self.root {
            self.root_dir.open_dir(OsStr::new(".")).ok()
        } else {
            self.reopen(parent).map(|(d, _)| d)
        };
        dir.and_then(|d| d.stat(raw).ok())
            .is_some_and(|m| local_id(&m) == id)
    }

    /// Opens a directory of this scan by its current path, if it is still the same directory.
    fn reopen(&self, id: LocalId) -> Option<(Dir, Meta)> {
        let rel = self.rel(id);
        let mut dir = self.root_dir.open_dir(OsStr::new(".")).ok()?;
        for part in rel.iter() {
            dir = dir.open_dir(part).ok()?;
        }
        let meta = dir.meta().ok()?;
        (meta.ino == self.dirs.get(&id)?.ino).then_some((dir, meta))
    }

    /// One comparison pass: reads every directory whose stamp changed again. `true` if any did.
    fn recheck(&mut self) -> Result<bool, ScanAbort> {
        // Directories changed so shortly before they were read that a later change could carry
        // the same timestamp are read once more, after their timestamp tick is surely over.
        let wait = self
            .dirs
            .values()
            .filter(|s| s.recent())
            .map(|s| s.mtime_ns.max(s.ctime_ns))
            .max();
        if let Some(t) = wait {
            let now = self.clock.now_ns();
            self.clock.sleep_until(
                t.saturating_add(STAMP_RESOLUTION_NS)
                    .min(now.saturating_add(STAMP_RESOLUTION_NS)),
            );
        }
        let ids: Vec<LocalId> = self.dirs.keys().copied().collect();
        let mut changed = false;
        for id in ids {
            let Some(stamp) = self.dirs.get(&id).copied() else {
                continue;
            };
            match self.reopen(id) {
                Some((_, meta)) if stamp.settled(&meta) => {}
                Some((dir, meta)) => {
                    changed = true;
                    self.forget_children(id);
                    self.read_dir(&dir, id, &meta, stamp.inside_opaque, id == self.root)?;
                }
                None => {
                    // Moved, replaced or gone: its new parent (whose stamp changed) reads it
                    // again; whatever is left of it here goes.
                    changed = true;
                    self.dirs.remove(&id);
                    if id == self.root {
                        return Err(ScanAbort::Root(io::Error::new(
                            io::ErrorKind::NotFound,
                            "Wurzel verändert",
                        )));
                    }
                }
            }
        }
        Ok(changed)
    }

    fn finish(mut self, stable: bool) -> Snapshot {
        for id in &self.duplicates {
            if let Some(i) = self.items.get_mut(id) {
                i.opaque = Some(OpaqueWhy::DuplicateId);
            }
        }
        // Items whose parent is no longer part of the scan were read under a directory that
        // moved away during the scan and was not found again: not reported (the next scan sees
        // them; the snapshot is not stable then).
        let reachable: HashSet<LocalId> = {
            let mut children: HashMap<LocalId, Vec<LocalId>> = HashMap::new();
            for (id, i) in &self.items {
                children.entry(i.parent).or_default().push(*id);
            }
            let mut ok = HashSet::new();
            let mut stack = vec![self.root];
            while let Some(at) = stack.pop() {
                for c in children.get(&at).into_iter().flatten() {
                    if ok.insert(*c) {
                        stack.push(*c);
                    }
                }
            }
            ok
        };
        let mut index = LocalIndex::new(self.root);
        let mut obs = Vec::new();
        let mut opaque = Vec::new();
        let mut ids: Vec<LocalId> = self.items.keys().copied().collect();
        ids.sort();
        for id in ids {
            if !reachable.contains(&id) {
                continue;
            }
            let i = &self.items[&id];
            index.insert(
                id,
                IndexEntry {
                    parent: i.parent,
                    raw: i.raw.clone(),
                    kind: i.kind,
                },
            );
            match i.opaque {
                Some(why) => opaque.push(Opaque {
                    id,
                    path: PathBuf::new(),
                    kind: Some(i.kind),
                    why,
                }),
                None => {
                    let name = i
                        .raw
                        .to_str()
                        .and_then(|s| Name::new(s).ok())
                        .expect("syncable names are valid");
                    obs.push(LocalObservation {
                        id,
                        entry: LocalEntry {
                            parent: i.parent,
                            name,
                            kind: i.kind,
                            fp: (i.kind == Kind::File).then_some(Fingerprint {
                                size: i.meta.size,
                                mtime_ns: i.meta.mtime_ns,
                                ctime_ns: i.meta.ctime_ns,
                            }),
                            content: i.content,
                        },
                    });
                }
            }
        }
        for o in &mut opaque {
            o.path = index.path(o.id).unwrap_or_default();
        }
        Snapshot {
            root: self.root,
            obs,
            opaque,
            blind: self.blind.into_iter().collect(),
            stable,
            index,
            retry_at_ns: self.retry_at_ns,
        }
    }
}

#[cfg(test)]
mod tests;

//! Executes the engine's local operations (ADR 0001 §4.1, ADR 0002 §7.6).
//!
//! The rules that keep user data safe:
//! - **Nothing is overwritten.** New names are taken with a no-replace rename; replacing content
//!   swaps a finished temporary file with the original atomically.
//! - **Proof before every irreversible step.** Before a swap or a move into the trash, an intent
//!   naming the inodes involved is written durably. After the step, the original is checked
//!   again; if it changed meanwhile (a program saved it), the step is undone.
//! - **Only what is proven the client's own is ever removed.** Originals go to the client's
//!   trash, never away; a temporary file is removed only if its inode proves it is ours.
//! - **Identity, not paths.** Every directory on the way is opened relative to its parent without
//!   following links and checked against the identity the scan recorded.
//!
//! `Precondition` means: something differs from what the engine planned with, nothing was
//! changed — the engine rescans and replans. `Error` means: something else failed, nothing was
//! changed (or a rescued original lies visibly next to the target).

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use xlrx_chunk::{Chunker, RACY_WINDOW_NS};
use xlrx_fs::{Dir, FileKind, Meta, RenameMode};
use xlrx_proto::name::{CLIENT_DIR, DOWNLOAD_TEMP_PREFIX, ignored};
use xlrx_proto::{FileContent, Name, NodeId};
use xlrx_sync::{Expected, Fingerprint, LocalId, LocalOp, LocalResult, OpId};

use super::id::local_id;
use super::index::LocalIndex;
use super::scan::Clock;
use super::store::{Intent, IntentKind, LocalStore, Phase, TrashItem, TrashReason};

/// Content is fetched in ranges of at most this size.
pub const RANGE: u64 = 8 * 1024 * 1024;

/// Where downloaded content comes from (the server, in tests a map).
pub trait ContentSource {
    /// Appends bytes `[offset, offset + len)` of the node's current content to `out`.
    fn read_range(
        &mut self,
        node: NodeId,
        offset: u64,
        len: u64,
        out: &mut Vec<u8>,
    ) -> io::Result<()>;
}

/// Points in the protocol where tests interfere: a program writing concurrently, or the process
/// dying (ADR 0002 §7.6, test hooks).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// The original passed its check (replace, delete).
    AfterPrecheck,
    /// Replacement content lies complete under the temporary name.
    ContentWritten,
    /// The intent for the swap or the move into the trash is durable.
    IntentWritten,
    AfterSwap,
    AfterTrash,
    /// Before a downloaded file is renamed to its target name.
    BeforeFinalRename,
    AfterFinalRename,
}

pub trait ExecHooks {
    fn at(&self, step: Step, target: &Path);
}

/// Outcome before it is mapped to a [`LocalResult`].
#[derive(Debug)]
enum Fail {
    Precondition,
    Error(String),
}

impl From<io::Error> for Fail {
    fn from(e: io::Error) -> Self {
        Fail::Error(e.to_string())
    }
}

impl From<rusqlite::Error> for Fail {
    fn from(e: rusqlite::Error) -> Self {
        Fail::Error(format!("local.sqlite: {e}"))
    }
}

fn missing(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    ) || e.raw_os_error() == Some(rustix::io::Errno::LOOP.raw_os_error())
}

fn exists(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AlreadyExists
}

fn fp_of(m: &Meta) -> Fingerprint {
    Fingerprint {
        size: m.size,
        mtime_ns: m.mtime_ns,
        ctime_ns: m.ctime_ns,
    }
}

/// Civil date (UTC) of a Unix time in ms, as `JJJJ-MM-TT` (Howard Hinnant's algorithm).
fn date(unix_ms: i64) -> String {
    let z = unix_ms.div_euclid(86_400_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Writes fetched content into the temporary file while the chunker reads (and hashes) it.
struct Tee<'a> {
    src: &'a mut dyn ContentSource,
    node: NodeId,
    file: &'a File,
    size: u64,
    pos: u64,
    buf: Vec<u8>,
    at: usize,
}

impl Read for Tee<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.at == self.buf.len() {
            if self.pos == self.size {
                return Ok(0);
            }
            let len = RANGE.min(self.size - self.pos);
            self.buf.clear();
            self.src
                .read_range(self.node, self.pos, len, &mut self.buf)?;
            if self.buf.len() as u64 != len {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Bereich unvollständig",
                ));
            }
            (&*self.file).write_all(&self.buf)?;
            self.pos += len;
            self.at = 0;
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// Executes local operations of one sync folder.
pub struct Executor<'a> {
    /// The sync folder.
    pub root: &'a Path,
    /// Raw names and parents from the last scan (and the executor's own changes).
    pub index: &'a LocalIndex,
    pub store: &'a LocalStore,
    /// Identities linked in S: a junk file in a folder being deleted is moved to the trash
    /// only if it is not linked.
    pub linked: &'a HashSet<LocalId>,
    pub clock: &'a dyn Clock,
    /// Unsafe window: a file changed this recently is rehashed before it is trusted
    /// (`max(RACY_WINDOW_NS, 2 × timestamp granularity)`).
    pub window_ns: i64,
    /// The volume folds case or normalization (APFS default).
    pub case_insensitive: bool,
    pub hooks: Option<&'a dyn ExecHooks>,
}

impl Executor<'_> {
    pub fn new<'a>(
        root: &'a Path,
        index: &'a LocalIndex,
        store: &'a LocalStore,
        linked: &'a HashSet<LocalId>,
        clock: &'a dyn Clock,
    ) -> Executor<'a> {
        Executor {
            root,
            index,
            store,
            linked,
            clock,
            window_ns: RACY_WINDOW_NS,
            case_insensitive: false,
            hooks: None,
        }
    }

    /// Executes one operation.
    pub fn run(&self, id: OpId, op: &LocalOp, src: &mut dyn ContentSource) -> LocalResult {
        self.run_explained(id, op, src).0
    }

    /// Like [`Executor::run`], with the reason of an `Error` (for the log and problem reports).
    pub fn run_explained(
        &self,
        id: OpId,
        op: &LocalOp,
        src: &mut dyn ContentSource,
    ) -> (LocalResult, Option<String>) {
        let r = match op {
            LocalOp::CreateDir { parent, name, .. } => self.create_dir(*parent, name),
            LocalOp::Download {
                parent,
                name,
                node,
                content,
                ..
            } => self.download(id, *parent, name, *node, *content, src),
            LocalOp::Replace {
                local,
                parent,
                name,
                expect,
                node,
                content,
                ..
            } => self.replace(id, *local, *parent, name, expect, *node, *content, src),
            LocalOp::Move {
                local,
                from_parent,
                from_name,
                parent,
                name,
                ..
            } => self.move_to(*local, *from_parent, from_name, *parent, name),
            LocalOp::DeleteFile {
                local,
                parent,
                name,
                expect,
                node,
            } => self.delete_file(id, *local, *parent, name, expect, *node),
            LocalOp::DeleteDir {
                local,
                parent,
                name,
                ..
            } => self.delete_dir(id, *local, *parent, name),
        };
        match r {
            Ok(done) => (done, None),
            Err(Fail::Precondition) => (LocalResult::Precondition, None),
            Err(Fail::Error(why)) => (LocalResult::Error, Some(why)),
        }
    }

    fn hook(&self, step: Step, target: &Path) {
        if let Some(h) = self.hooks {
            h.at(step, target);
        }
    }

    fn now_ms(&self) -> i64 {
        self.clock.now_ns().div_euclid(1_000_000)
    }

    fn unsafe_time(&self, t: i64) -> bool {
        t > self.clock.now_ns().saturating_sub(self.window_ns)
    }

    /// Opens the directory `id` along the identities the scan recorded.
    fn open(&self, id: LocalId) -> Result<(Dir, PathBuf), Fail> {
        let root = Dir::open(self.root)?;
        if Some(local_id(&root.meta()?)) != self.index.root() {
            return Err(Fail::Error("Wurzel ausgetauscht".into()));
        }
        let chain = self.index.chain(id).ok_or(Fail::Precondition)?;
        let mut dir = root;
        let mut path = PathBuf::new();
        for (cid, raw) in chain {
            dir = match dir.open_dir(raw) {
                Ok(d) => d,
                Err(e) if missing(&e) => return Err(Fail::Precondition),
                Err(e) => return Err(e.into()),
            };
            if local_id(&dir.meta()?) != cid {
                return Err(Fail::Precondition);
            }
            path.push(raw);
        }
        Ok((dir, path))
    }

    /// The raw name of `local` if it lies in `parent` under `name` (in NFC).
    fn raw_name(&self, local: LocalId, parent: LocalId, name: &Name) -> Result<&OsStr, Fail> {
        let e = self.index.get(local).ok_or(Fail::Precondition)?;
        let nfc = e.raw.to_str().and_then(|s| Name::new(s).ok());
        if e.parent != parent || nfc.as_ref() != Some(name) {
            return Err(Fail::Precondition);
        }
        Ok(e.raw.as_os_str())
    }

    /// Hashes a file through a fresh descriptor, checking it did not change while being read.
    fn rehash(&self, dir: &Dir, raw: &OsStr) -> Result<Option<FileContent>, Fail> {
        let f = dir.open_file(raw)?;
        let before = xlrx_fs::meta_of(&f)?;
        let digest = Chunker::new().digest_reader(&f)?;
        let after = xlrx_fs::meta_of(&f)?;
        Ok(
            (fp_of(&before) == fp_of(&after) && digest.content.size == after.size)
                .then_some(digest.content),
        )
    }

    /// The original is still exactly what the engine planned with.
    fn precheck(
        &self,
        dir: &Dir,
        raw: &OsStr,
        local: LocalId,
        expect: &Expected,
    ) -> Result<Meta, Fail> {
        let m = match dir.stat(raw) {
            Ok(m) => m,
            Err(e) if missing(&e) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        };
        if m.kind != FileKind::File || m.nlink != 1 || local_id(&m) != local {
            return Err(Fail::Precondition);
        }
        if fp_of(&m) != expect.fp {
            return Err(Fail::Precondition);
        }
        if self.unsafe_time(m.mtime_ns.max(m.ctime_ns))
            && self.rehash(dir, raw)? != Some(expect.content)
        {
            return Err(Fail::Precondition);
        }
        Ok(m)
    }

    /// After the client's own rename: the moved object is still the original as expected.
    /// Never compares ctime: a rename or swap changes it.
    fn postcheck(
        &self,
        dir: &Dir,
        raw: &OsStr,
        orig: &Meta,
        expect: &Expected,
    ) -> Result<bool, Fail> {
        let m = match dir.stat(raw) {
            Ok(m) => m,
            Err(e) if missing(&e) => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        if m.ino != orig.ino
            || m.kind != FileKind::File
            || m.size != expect.fp.size
            || m.mtime_ns != expect.fp.mtime_ns
        {
            return Ok(false);
        }
        if self.unsafe_time(expect.fp.mtime_ns) {
            return Ok(self.rehash(dir, raw)? == Some(expect.content));
        }
        Ok(true)
    }

    /// A name for the client's temporary file next to the target.
    fn temp_name(&self, id: OpId) -> OsString {
        OsString::from(format!(
            "{DOWNLOAD_TEMP_PREFIX}{}-{:x}",
            id.0,
            self.clock.now_ns() as u64 & 0xffff_ffff
        ))
    }

    /// Removes the client's temporary file, if (and only if) it still is the one it created.
    fn remove_own(&self, dir: &Dir, temp: &OsStr, temp_ino: u64) -> Result<(), Fail> {
        match dir.stat(temp) {
            Ok(m) if m.ino == temp_ino && m.kind == FileKind::File => {
                dir.unlink(temp)?;
                Ok(())
            }
            Ok(_) => Err(Fail::Error(
                "Temporäre Datei ist nicht mehr die eigene".into(),
            )),
            Err(e) if missing(&e) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    /// The trash directory for today, created if needed; and a fresh name in it.
    fn trash(&self, id: OpId) -> Result<(Dir, PathBuf), Fail> {
        let root = Dir::open(self.root)?;
        let mut dir = root;
        let day = date(self.now_ms());
        for part in [CLIENT_DIR, "trash", day.as_str()] {
            match dir.mkdir(OsStr::new(part)) {
                Ok(()) => dir.sync()?,
                Err(e) if exists(&e) => {}
                Err(e) => return Err(e.into()),
            }
            dir = dir.open_dir(OsStr::new(part))?;
        }
        let name = PathBuf::from(&day).join(format!(
            "{}-{:x}",
            id.0,
            self.clock.now_ns() as u64 & 0xffff_ffff_ffff
        ));
        Ok((dir, name))
    }

    /// Moves `raw` from `from` into `to` under "<stem of like> (gerettet <date>)": visible, never
    /// overwriting.
    fn rescue(&self, from: &Dir, raw: &OsStr, to: &Dir, like: &OsStr) -> Result<(), Fail> {
        let day = date(self.now_ms());
        for n in 1..100 {
            let suffix = if n == 1 {
                format!("gerettet {day}")
            } else {
                format!("gerettet {day} {n}")
            };
            let name: OsString = match like.to_str().and_then(|s| Name::new(s).ok()) {
                Some(nm) => nm.with_suffix(&suffix).as_str().into(),
                None => {
                    let mut o = like.to_owned();
                    o.push(format!(" ({suffix})"));
                    o
                }
            };
            match from.rename(raw, to, &name, RenameMode::NoReplace) {
                Ok(()) => {
                    to.sync()?;
                    from.sync()?;
                    return Ok(());
                }
                Err(e) if exists(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Fail::Error("Kein freier Name zum Retten".into()))
    }

    fn create_dir(&self, parent: LocalId, name: &Name) -> Result<LocalResult, Fail> {
        let (dir, _) = self.open(parent)?;
        let target = OsStr::new(name.as_str());
        match dir.stat(target) {
            Ok(_) => return Err(Fail::Precondition),
            Err(e) if missing(&e) => {}
            Err(e) => return Err(e.into()),
        }
        match dir.mkdir(target) {
            Ok(()) => {}
            Err(e) if exists(&e) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        }
        dir.sync()?;
        let created = dir.open_dir(target)?.meta()?;
        Ok(LocalResult::Done {
            id: local_id(&created),
            fp: None,
        })
    }

    /// Writes the content of `node` into a new temporary file `temp` in `dir`; returns the open
    /// file, its sealed metadata and the content as hashed while writing.
    #[allow(clippy::too_many_arguments)]
    fn write_temp(
        &self,
        dir: &Dir,
        temp: &OsStr,
        intent: &mut Intent,
        node: NodeId,
        size: u64,
        src: &mut dyn ContentSource,
    ) -> Result<(File, Meta, FileContent), Fail> {
        intent.id = self.store.add_intent(intent)?;
        let file = dir.create_new(temp)?;
        let meta = xlrx_fs::meta_of(&file)?;
        intent.temp_ino = Some(meta.ino);
        self.store.update_intent(intent)?;
        let tee = Tee {
            src,
            node,
            file: &file,
            size,
            pos: 0,
            buf: Vec::new(),
            at: 0,
        };
        let digest = Chunker::new().digest_reader(tee);
        let digest = match digest {
            Ok(d) => d,
            Err(e) => {
                self.remove_own(dir, temp, meta.ino)?;
                self.store.remove_intent(intent.id)?;
                return Err(e.into());
            }
        };
        xlrx_fs::sync_file(&file)?;
        let sealed = xlrx_fs::meta_of(&file)?;
        Ok((file, sealed, digest.content))
    }

    fn download(
        &self,
        id: OpId,
        parent: LocalId,
        name: &Name,
        node: NodeId,
        content: FileContent,
        src: &mut dyn ContentSource,
    ) -> Result<LocalResult, Fail> {
        let (dir, dir_path) = self.open(parent)?;
        let target = OsStr::new(name.as_str());
        match dir.stat(target) {
            Ok(_) => return Err(Fail::Precondition),
            Err(e) if missing(&e) => {}
            Err(e) => return Err(e.into()),
        }
        let temp = self.temp_name(id);
        let mut intent = Intent {
            id: 0,
            op_id: id.0,
            kind: IntentKind::Download,
            dir_path: dir_path.as_os_str().as_bytes().to_vec(),
            dir_ino: dir.meta()?.ino,
            target_name: target.as_bytes().to_vec(),
            temp_name: Some(temp.as_bytes().to_vec()),
            temp_ino: None,
            orig_ino: None,
            trash_name: None,
            expect: None,
            phase: Phase::Writing,
            created_ms: self.now_ms(),
        };
        let (file, sealed, got) =
            self.write_temp(&dir, &temp, &mut intent, node, content.size, src)?;
        if got != content {
            // The server's content changed since the engine planned (or arrived broken).
            self.remove_own(&dir, &temp, sealed.ino)?;
            self.store.remove_intent(intent.id)?;
            return Err(Fail::Error("Inhalt passt nicht zum Hash".into()));
        }
        self.hook(Step::BeforeFinalRename, &dir_path.join(target));
        match dir.rename(&temp, &dir, target, RenameMode::NoReplace) {
            Ok(()) => {}
            Err(e) if exists(&e) => {
                self.remove_own(&dir, &temp, sealed.ino)?;
                self.store.remove_intent(intent.id)?;
                return Err(Fail::Precondition);
            }
            Err(e) => return Err(e.into()),
        }
        dir.sync()?;
        self.hook(Step::AfterFinalRename, &dir_path.join(target));
        let fp = self.result_fp(&file, &sealed, got)?;
        self.store.remove_intent(intent.id)?;
        Ok(LocalResult::Done {
            id: local_id(&sealed),
            fp,
        })
    }

    /// The fingerprint of written content, from the open descriptor (never by name): only if
    /// size and mtime are still what they were after syncing, and then also cached.
    fn result_fp(
        &self,
        file: &File,
        sealed: &Meta,
        content: FileContent,
    ) -> Result<Option<Fingerprint>, Fail> {
        let now = xlrx_fs::meta_of(file)?;
        if (now.size, now.mtime_ns) != (sealed.size, sealed.mtime_ns) {
            return Ok(None);
        }
        let fp = fp_of(&now);
        self.store
            .remember(local_id(&now), fp, self.clock.now_ns(), content)?;
        Ok(Some(fp))
    }

    #[allow(clippy::too_many_arguments)]
    fn replace(
        &self,
        id: OpId,
        local: LocalId,
        parent: LocalId,
        name: &Name,
        expect: &Expected,
        node: NodeId,
        content: FileContent,
        src: &mut dyn ContentSource,
    ) -> Result<LocalResult, Fail> {
        let (dir, dir_path) = self.open(parent)?;
        let raw = self.raw_name(local, parent, name)?.to_owned();
        let target_path = dir_path.join(&raw);
        let orig = self.precheck(&dir, &raw, local, expect)?;
        self.hook(Step::AfterPrecheck, &target_path);
        let temp = self.temp_name(id);
        let mut intent = Intent {
            id: 0,
            op_id: id.0,
            kind: IntentKind::Replace,
            dir_path: dir_path.as_os_str().as_bytes().to_vec(),
            dir_ino: dir.meta()?.ino,
            target_name: raw.as_bytes().to_vec(),
            temp_name: Some(temp.as_bytes().to_vec()),
            temp_ino: None,
            orig_ino: Some(orig.ino),
            trash_name: None,
            expect: Some(*expect),
            phase: Phase::Writing,
            created_ms: self.now_ms(),
        };
        let (file, sealed, got) =
            self.write_temp(&dir, &temp, &mut intent, node, content.size, src)?;
        let abandon = |intent_id: i64, why: Fail| -> Result<LocalResult, Fail> {
            self.remove_own(&dir, &temp, sealed.ino)?;
            self.store.remove_intent(intent_id)?;
            Err(why)
        };
        if got != content {
            return abandon(intent.id, Fail::Error("Inhalt passt nicht zum Hash".into()));
        }
        intent.phase = Phase::Prepared;
        self.store.update_intent(&intent)?;
        self.hook(Step::ContentWritten, &target_path);
        // Writing took time: the original must still be unchanged right before the swap.
        if let Err(f) = self.precheck(&dir, &raw, local, expect) {
            return abandon(intent.id, f);
        }
        intent.phase = Phase::Swapping;
        self.store.update_intent(&intent)?;
        self.hook(Step::IntentWritten, &target_path);
        match dir.rename(&temp, &dir, &raw, RenameMode::Exchange) {
            Ok(()) => {}
            // The original vanished in the last moment: nothing was swapped.
            Err(e) if missing(&e) => return abandon(intent.id, Fail::Precondition),
            Err(e) => return abandon(intent.id, e.into()),
        }
        self.hook(Step::AfterSwap, &target_path);
        if !self.postcheck(&dir, &temp, &orig, expect)? {
            // Someone wrote to the original between the check and the swap: undo, keep theirs.
            let still_ours = dir.stat(&raw).is_ok_and(|m| m.ino == sealed.ino);
            if still_ours && dir.rename(&temp, &dir, &raw, RenameMode::Exchange).is_ok() {
                dir.sync()?;
                self.remove_own(&dir, &temp, sealed.ino)?;
                self.store.remove_intent(intent.id)?;
                return Err(Fail::Precondition);
            }
            self.rescue(&dir, &temp, &dir, &raw)?;
            self.store.remove_intent(intent.id)?;
            return Err(Fail::Error("Original gerettet".into()));
        }
        // The original (now under the temporary name) goes to the trash, never away.
        let (trash, trash_name) = self.trash(id)?;
        let leaf = trash_name.file_name().expect("Name").to_owned();
        dir.rename(&temp, &trash, &leaf, RenameMode::NoReplace)?;
        trash.sync()?;
        dir.sync()?;
        self.store.add_trash(&TrashItem {
            id: 0,
            trash_name: trash_name.as_os_str().as_bytes().to_vec(),
            orig_path: target_path.as_os_str().as_bytes().to_vec(),
            ino: Some(orig.ino),
            node: Some(node.0),
            content: Some(expect.content),
            reason: TrashReason::Replaced,
            trashed_ms: self.now_ms(),
        })?;
        self.hook(Step::AfterTrash, &target_path);
        let fp = self.result_fp(&file, &sealed, got)?;
        self.store.remove_intent(intent.id)?;
        Ok(LocalResult::Done {
            id: local_id(&sealed),
            fp,
        })
    }

    fn move_to(
        &self,
        local: LocalId,
        from_parent: LocalId,
        from_name: &Name,
        parent: LocalId,
        name: &Name,
    ) -> Result<LocalResult, Fail> {
        let (from_dir, _) = self.open(from_parent)?;
        let raw = self.raw_name(local, from_parent, from_name)?;
        let m = match from_dir.stat(raw) {
            Ok(m) => m,
            Err(e) if missing(&e) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        };
        if local_id(&m) != local {
            return Err(Fail::Precondition);
        }
        let (to_dir, _) = self.open(parent)?;
        let target = OsStr::new(name.as_str());
        // A name only changing its spelling of the same object: on a folding volume the target
        // "exists" as the object itself.
        let same_object = |d: &Dir| d.stat(target).is_ok_and(|t| local_id(&t) == local);
        match to_dir.stat(target) {
            Err(e) if missing(&e) => {}
            Ok(_) if from_parent == parent && self.case_insensitive && same_object(&to_dir) => {}
            Ok(_) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        }
        match from_dir.rename(raw, &to_dir, target, RenameMode::NoReplace) {
            Ok(()) => {}
            Err(e) if exists(&e) => {
                if from_parent == parent && self.case_insensitive && same_object(&to_dir) {
                    // Proven the same entry: a plain rename cannot overwrite anything here.
                    from_dir.rename(raw, &to_dir, target, RenameMode::Plain)?;
                } else {
                    return Err(Fail::Precondition);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::InvalidInput => return Err(Fail::Precondition),
            Err(e) if missing(&e) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        }
        to_dir.sync()?;
        if from_parent != parent {
            from_dir.sync()?;
        }
        let now = to_dir.stat(target)?;
        Ok(LocalResult::Done {
            id: local,
            fp: (now.kind == FileKind::File && local_id(&now) == local).then(|| fp_of(&now)),
        })
    }

    fn delete_file(
        &self,
        id: OpId,
        local: LocalId,
        parent: LocalId,
        name: &Name,
        expect: &Expected,
        node: NodeId,
    ) -> Result<LocalResult, Fail> {
        let (dir, dir_path) = self.open(parent)?;
        let raw = self.raw_name(local, parent, name)?.to_owned();
        let target_path = dir_path.join(&raw);
        let orig = self.precheck(&dir, &raw, local, expect)?;
        self.hook(Step::AfterPrecheck, &target_path);
        let (trash, trash_name) = self.trash(id)?;
        let leaf = trash_name.file_name().expect("Name").to_owned();
        let intent = Intent {
            id: 0,
            op_id: id.0,
            kind: IntentKind::Delete,
            dir_path: dir_path.as_os_str().as_bytes().to_vec(),
            dir_ino: dir.meta()?.ino,
            target_name: raw.as_bytes().to_vec(),
            temp_name: None,
            temp_ino: None,
            orig_ino: Some(orig.ino),
            trash_name: Some(trash_name.as_os_str().as_bytes().to_vec()),
            expect: Some(*expect),
            phase: Phase::Deleting,
            created_ms: self.now_ms(),
        };
        let intent_id = self.store.add_intent(&intent)?;
        self.hook(Step::IntentWritten, &target_path);
        match dir.rename(&raw, &trash, &leaf, RenameMode::NoReplace) {
            Ok(()) => {}
            Err(e) if missing(&e) => {
                self.store.remove_intent(intent_id)?;
                return Err(Fail::Precondition);
            }
            Err(e) => {
                self.store.remove_intent(intent_id)?;
                return Err(e.into());
            }
        }
        trash.sync()?;
        dir.sync()?;
        self.hook(Step::AfterTrash, &target_path);
        if !self.postcheck(&trash, &leaf, &orig, expect)? {
            // Changed at the last moment: back where it was, or visibly next to it.
            match trash.rename(&leaf, &dir, &raw, RenameMode::NoReplace) {
                Ok(()) => dir.sync()?,
                Err(e) if exists(&e) => self.rescue(&trash, &leaf, &dir, &raw)?,
                Err(e) => return Err(e.into()),
            }
            self.store.remove_intent(intent_id)?;
            return Err(Fail::Precondition);
        }
        self.store.add_trash(&TrashItem {
            id: 0,
            trash_name: trash_name.as_os_str().as_bytes().to_vec(),
            orig_path: target_path.as_os_str().as_bytes().to_vec(),
            ino: Some(orig.ino),
            node: Some(node.0),
            content: Some(expect.content),
            reason: TrashReason::Delete,
            trashed_ms: self.now_ms(),
        })?;
        self.store.remove_intent(intent_id)?;
        Ok(LocalResult::Done {
            id: local,
            fp: None,
        })
    }

    fn delete_dir(
        &self,
        id: OpId,
        local: LocalId,
        parent: LocalId,
        name: &Name,
    ) -> Result<LocalResult, Fail> {
        let (dir, dir_path) = self.open(parent)?;
        let raw = self.raw_name(local, parent, name)?.to_owned();
        let sub = match dir.open_dir(&raw) {
            Ok(d) => d,
            Err(e) if missing(&e) => return Err(Fail::Precondition),
            Err(e) => return Err(e.into()),
        };
        if local_id(&sub.meta()?) != local {
            return Err(Fail::Precondition);
        }
        // Left-over junk files (never directories, never linked ones) go to the trash; anything
        // else keeps the folder (ENOTEMPTY below).
        for (entry, _) in sub.entries()? {
            let Some(s) = entry.to_str() else { continue };
            if !ignored(s) {
                continue;
            }
            let Ok(m) = sub.stat(&entry) else { continue };
            if m.kind != FileKind::File || self.linked.contains(&local_id(&m)) {
                continue;
            }
            let (trash, trash_name) = self.trash(id)?;
            let leaf = trash_name.file_name().expect("Name").to_owned();
            sub.rename(&entry, &trash, &leaf, RenameMode::NoReplace)?;
            trash.sync()?;
            self.store.add_trash(&TrashItem {
                id: 0,
                trash_name: trash_name.as_os_str().as_bytes().to_vec(),
                orig_path: dir_path
                    .join(&raw)
                    .join(&entry)
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
                ino: Some(m.ino),
                node: None,
                content: None,
                reason: TrashReason::Junk,
                trashed_ms: self.now_ms(),
            })?;
        }
        match dir.rmdir(&raw) {
            Ok(()) => {}
            Err(e) if missing(&e) => return Err(Fail::Precondition),
            Err(e)
                if e.raw_os_error() == Some(rustix::io::Errno::NOTEMPTY.raw_os_error())
                    || e.raw_os_error() == Some(rustix::io::Errno::EXIST.raw_os_error()) =>
            {
                return Err(Fail::Precondition);
            }
            Err(e) => return Err(e.into()),
        }
        dir.sync()?;
        Ok(LocalResult::Done {
            id: local,
            fp: None,
        })
    }
}

/// What [`Executor::recover`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Own temporary files removed (proven by their inode).
    pub removed: usize,
    /// Originals or unproven temporaries moved to the trash.
    pub trashed: usize,
    /// Originals put back where they were.
    pub restored: usize,
    /// Files put visibly next to their place ("(gerettet …)").
    pub rescued: usize,
    /// Intents that had nothing left to do.
    pub dropped: usize,
    /// Files in the trash without a record, adopted.
    pub adopted: usize,
}

impl Executor<'_> {
    /// Finishes or undoes what a crash interrupted, from the intents (ADR 0002 §7.6). Runs before
    /// the first scan. A temporary file that could be a swapped original is never removed.
    pub fn recover(&self) -> Result<Recovered, String> {
        let mut r = Recovered::default();
        let intents = self.store.intents().map_err(|e| e.to_string())?;
        for i in intents {
            self.recover_one(&i, &mut r).map_err(|f| format!("{f:?}"))?;
            self.store.remove_intent(i.id).map_err(|e| e.to_string())?;
        }
        r.adopted = self.adopt_trash().map_err(|f| format!("{f:?}"))?;
        Ok(r)
    }

    /// Opens a directory by its recorded path, if it is still the recorded one.
    fn open_path(&self, rel: &[u8], ino: u64) -> Option<Dir> {
        let mut dir = Dir::open(self.root).ok()?;
        for part in Path::new(OsStr::from_bytes(rel)).iter() {
            dir = dir.open_dir(part).ok()?;
        }
        (dir.meta().ok()?.ino == ino).then_some(dir)
    }

    fn recover_one(&self, i: &Intent, r: &mut Recovered) -> Result<(), Fail> {
        let Some(dir) = self.open_path(&i.dir_path, i.dir_ino) else {
            // The folder moved or vanished: whatever is left there is reported by the scan
            // (temporary names are never uploaded) and swept into the trash later.
            r.dropped += 1;
            return Ok(());
        };
        let target = OsStr::from_bytes(&i.target_name);
        let temp = i.temp_name.as_deref().map(OsStr::from_bytes);
        let stat = |name: &OsStr| dir.stat(name).ok();
        match (i.kind, i.phase) {
            (IntentKind::Download | IntentKind::Replace, Phase::Writing | Phase::Prepared) => {
                // No swap yet: the temporary file can only hold content the client wrote.
                let Some(temp) = temp else {
                    r.dropped += 1;
                    return Ok(());
                };
                match (stat(temp), i.temp_ino) {
                    (Some(m), Some(ino)) if m.ino == ino => {
                        dir.unlink(temp)?;
                        dir.sync()?;
                        r.removed += 1;
                    }
                    (Some(m), _) if m.kind == FileKind::File => {
                        // Created, inode not recorded yet: not proven, so kept in the trash.
                        self.to_trash(&dir, temp, i, m.ino, None, TrashReason::Recovered)?;
                        r.trashed += 1;
                    }
                    _ => r.dropped += 1,
                }
            }
            (IntentKind::Replace, Phase::Swapping) => {
                let Some(temp) = temp else {
                    r.dropped += 1;
                    return Ok(());
                };
                let Some(m) = stat(temp) else {
                    // Swapped and already moved on (into the trash) or swapped back and removed.
                    r.dropped += 1;
                    return Ok(());
                };
                if Some(m.ino) == i.temp_ino {
                    // Not swapped (or swapped back): our own new content.
                    dir.unlink(temp)?;
                    dir.sync()?;
                    r.removed += 1;
                } else if Some(m.ino) == i.orig_ino {
                    // Swapped, check missing: the original as expected goes to the trash,
                    // anything else stays visible.
                    let expect = i.expect.ok_or_else(|| Fail::Error("expect fehlt".into()))?;
                    if self.postcheck(&dir, temp, &m, &expect)? {
                        self.to_trash(
                            &dir,
                            temp,
                            i,
                            m.ino,
                            Some(expect.content),
                            TrashReason::Replaced,
                        )?;
                        r.trashed += 1;
                    } else {
                        self.rescue(&dir, temp, &dir, target)?;
                        r.rescued += 1;
                    }
                } else {
                    // Neither ours nor the original: a program's save that got swapped in.
                    self.rescue(&dir, temp, &dir, target)?;
                    r.rescued += 1;
                }
            }
            (IntentKind::Delete, Phase::Deleting) => {
                let Some(trash_name) = i.trash_name.as_deref() else {
                    r.dropped += 1;
                    return Ok(());
                };
                let trash_path = Path::new(OsStr::from_bytes(trash_name));
                let (Some(day), Some(leaf)) = (trash_path.parent(), trash_path.file_name()) else {
                    r.dropped += 1;
                    return Ok(());
                };
                let trash = Dir::open(self.root)
                    .and_then(|d| d.open_dir(OsStr::new(CLIENT_DIR)))
                    .and_then(|d| d.open_dir(OsStr::new("trash")))
                    .and_then(|d| d.open_dir(day.as_os_str()));
                let in_trash = trash
                    .as_ref()
                    .ok()
                    .and_then(|t| t.stat(leaf).ok().map(|m| (t, m)));
                match in_trash {
                    Some((t, m)) if Some(m.ino) == i.orig_ino => {
                        let expect = i.expect.ok_or_else(|| Fail::Error("expect fehlt".into()))?;
                        if self.postcheck(t, leaf, &m, &expect)? {
                            self.store.add_trash(&TrashItem {
                                id: 0,
                                trash_name: trash_name.to_vec(),
                                orig_path: Path::new(OsStr::from_bytes(&i.dir_path))
                                    .join(target)
                                    .as_os_str()
                                    .as_bytes()
                                    .to_vec(),
                                ino: Some(m.ino),
                                node: None,
                                content: Some(expect.content),
                                reason: TrashReason::Delete,
                                trashed_ms: self.now_ms(),
                            })?;
                            r.trashed += 1;
                        } else {
                            match t.rename(leaf, &dir, target, RenameMode::NoReplace) {
                                Ok(()) => {
                                    dir.sync()?;
                                    r.restored += 1;
                                }
                                Err(e) if exists(&e) => {
                                    self.rescue(t, leaf, &dir, target)?;
                                    r.rescued += 1;
                                }
                                Err(e) => return Err(e.into()),
                            }
                        }
                    }
                    // Not moved yet: the original is still in place.
                    _ => r.dropped += 1,
                }
            }
            _ => r.dropped += 1,
        }
        Ok(())
    }

    fn to_trash(
        &self,
        dir: &Dir,
        name: &OsStr,
        i: &Intent,
        ino: u64,
        content: Option<FileContent>,
        reason: TrashReason,
    ) -> Result<(), Fail> {
        let (trash, trash_name) = self.trash(OpId(i.op_id))?;
        let leaf = trash_name.file_name().expect("Name").to_owned();
        dir.rename(name, &trash, &leaf, RenameMode::NoReplace)?;
        trash.sync()?;
        dir.sync()?;
        self.store.add_trash(&TrashItem {
            id: 0,
            trash_name: trash_name.as_os_str().as_bytes().to_vec(),
            orig_path: Path::new(OsStr::from_bytes(&i.dir_path))
                .join(OsStr::from_bytes(&i.target_name))
                .as_os_str()
                .as_bytes()
                .to_vec(),
            ino: Some(ino),
            node: None,
            content,
            reason,
            trashed_ms: self.now_ms(),
        })?;
        Ok(())
    }

    /// Files in the trash directory without a record (a crash between the move and the record)
    /// are adopted, never removed.
    fn adopt_trash(&self) -> Result<usize, Fail> {
        let Ok(trash) = Dir::open(self.root)
            .and_then(|d| d.open_dir(OsStr::new(CLIENT_DIR)))
            .and_then(|d| d.open_dir(OsStr::new("trash")))
        else {
            return Ok(0);
        };
        let known: HashSet<Vec<u8>> = self
            .store
            .trash()?
            .into_iter()
            .map(|t| t.trash_name)
            .collect();
        let mut adopted = 0;
        for (day, _) in trash.entries()? {
            let Ok(d) = trash.open_dir(&day) else {
                continue;
            };
            for (leaf, _) in d.entries()? {
                let rel = Path::new(&day).join(&leaf);
                if known.contains(rel.as_os_str().as_bytes()) {
                    continue;
                }
                let ino = d.stat(&leaf).ok().map(|m| m.ino);
                self.store.add_trash(&TrashItem {
                    id: 0,
                    trash_name: rel.as_os_str().as_bytes().to_vec(),
                    orig_path: Vec::new(),
                    ino,
                    node: None,
                    content: None,
                    reason: TrashReason::Recovered,
                    trashed_ms: self.now_ms(),
                })?;
                adopted += 1;
            }
        }
        Ok(adopted)
    }
}

#[cfg(test)]
mod tests;

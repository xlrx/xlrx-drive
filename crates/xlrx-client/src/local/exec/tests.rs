use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::panic::{AssertUnwindSafe, catch_unwind};

use xlrx_proto::Rev;
use xlrx_sync::LocalEntry;

use super::*;
use crate::local::scan::{ScanSettings, SystemClock, full_scan};

/// A clock an hour ahead: every file is old enough to be trusted without rehashing.
struct Later(Cell<i64>);

impl Later {
    fn new() -> Self {
        Self(Cell::new(SystemClock.now_ns() + 3_600_000_000_000))
    }
}

impl Clock for Later {
    fn now_ns(&self) -> i64 {
        self.0.get()
    }
    fn sleep_until(&self, at: i64) {
        self.0.set(self.0.get().max(at));
    }
}

#[derive(Default)]
struct Source(HashMap<NodeId, Vec<u8>>);

impl ContentSource for Source {
    fn read_range(
        &mut self,
        node: NodeId,
        offset: u64,
        len: u64,
        out: &mut Vec<u8>,
    ) -> io::Result<()> {
        let data = self
            .0
            .get(&node)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Knoten fehlt"))?;
        let end = (offset + len).min(data.len() as u64);
        out.extend_from_slice(&data[offset as usize..end as usize]);
        Ok(())
    }
}

fn content(data: &[u8]) -> FileContent {
    Chunker::new().digest_slice(data).content
}

struct Setup {
    _t: tempfile::TempDir,
    root: PathBuf,
    store: LocalStore,
    clock: Later,
}

/// Raw names and identities as the scanner sees them now.
struct View {
    index: LocalIndex,
    obs: BTreeMap<PathBuf, (LocalId, LocalEntry)>,
}

impl Setup {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let store = LocalStore::open(&t.path().join("local.sqlite")).unwrap();
        let root = t.path().join("root");
        fs::create_dir(&root).unwrap();
        Self {
            _t: t,
            root,
            store,
            clock: Later::new(),
        }
    }

    /// Like [`Setup::new`], with the sync folder below `base` (the local store stays outside).
    fn below(base: &Path) -> (Self, tempfile::TempDir) {
        let small = tempfile::tempdir_in(base).unwrap();
        let s = Self {
            root: small.path().to_owned(),
            ..Self::new()
        };
        (s, small)
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn view(&self) -> View {
        let snap = full_scan(
            &self.root(),
            &self.store,
            &HashSet::new(),
            ScanSettings::default(),
            &self.clock,
            None,
        )
        .unwrap();
        let obs = snap
            .obs
            .iter()
            .map(|o| (snap.index.path(o.id).unwrap(), (o.id, o.entry.clone())))
            .collect();
        View {
            index: snap.index,
            obs,
        }
    }

    fn exec<'a>(&'a self, v: &'a View, linked: &'a HashSet<LocalId>) -> Executor<'a> {
        Executor::new(&self.root, &v.index, &self.store, linked, &self.clock)
    }

    /// Every file below the root (without the client directory) with its content, and every file
    /// in the trash.
    fn inventory(&self) -> (BTreeMap<String, Vec<u8>>, Vec<Vec<u8>>) {
        let mut files = BTreeMap::new();
        let mut trash = Vec::new();
        let mut stack = vec![self.root()];
        while let Some(d) = stack.pop() {
            for e in fs::read_dir(&d).unwrap() {
                let e = e.unwrap();
                let path = e.path();
                let rel = path.strip_prefix(self.root()).unwrap().to_owned();
                if e.file_type().unwrap().is_dir() {
                    stack.push(path);
                } else if rel.starts_with(CLIENT_DIR) {
                    trash.push(fs::read(&path).unwrap());
                } else {
                    files.insert(rel.to_string_lossy().into_owned(), fs::read(&path).unwrap());
                }
            }
        }
        (files, trash)
    }
}

fn root_id(v: &View) -> LocalId {
    v.index.root().unwrap()
}

fn expect_of(e: &LocalEntry) -> Expected {
    Expected {
        fp: e.fp.unwrap(),
        content: e.content.unwrap(),
    }
}

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

#[test]
fn ordner_anlegen() {
    let s = Setup::new();
    let v = s.view();
    let linked = HashSet::new();
    let x = s.exec(&v, &linked);
    let op = LocalOp::CreateDir {
        parent: root_id(&v),
        name: name("Projekte"),
        node: NodeId(5),
        node_parent: NodeId(1),
    };
    let r = x.run(OpId(1), &op, &mut Source::default());
    assert!(matches!(r, LocalResult::Done { fp: None, .. }), "{r:?}");
    assert!(s.p("Projekte").is_dir());
    // The name is taken now: nothing is changed.
    assert_eq!(
        x.run(OpId(2), &op, &mut Source::default()),
        LocalResult::Precondition
    );
}

#[test]
fn herunterladen() {
    let s = Setup::new();
    fs::create_dir(s.p("Ziel")).unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let x = s.exec(&v, &linked);
    let dir = v.obs[&PathBuf::from("Ziel")].0;
    // Larger than one range: fetched in parts.
    let data: Vec<u8> = (0..RANGE as usize + 12_345)
        .map(|i| (i * 7 % 251) as u8)
        .collect();
    let mut src = Source::default();
    src.0.insert(NodeId(9), data.clone());
    src.0.insert(NodeId(10), Vec::new());
    let download = |n: &str, node: u64, c: FileContent| LocalOp::Download {
        parent: dir,
        name: name(n),
        node: NodeId(node),
        node_parent: NodeId(1),
        rev: Rev(1),
        content: c,
    };
    let r = x.run(OpId(3), &download("gross.bin", 9, content(&data)), &mut src);
    let LocalResult::Done { id, fp } = r else {
        panic!("{r:?}")
    };
    assert_eq!(fs::read(s.p("Ziel/gross.bin")).unwrap(), data);
    let m = Dir::open(&s.p("Ziel"))
        .unwrap()
        .stat(OsStr::new("gross.bin"))
        .unwrap();
    assert_eq!(id, local_id(&m));
    assert_eq!(fp, Some(fp_of(&m)));
    assert!(s.store.intents().unwrap().is_empty());
    // An empty file: nothing to fetch.
    let r = x.run(OpId(4), &download("leer", 10, content(b"")), &mut src);
    assert!(matches!(r, LocalResult::Done { .. }), "{r:?}");
    assert_eq!(fs::read(s.p("Ziel/leer")).unwrap(), b"");
    // The server's content differs from the plan: nothing is left behind.
    let r = x.run(
        OpId(5),
        &download("falsch", 9, content(b"anders")),
        &mut src,
    );
    assert_eq!(r, LocalResult::Error);
    // The name is taken: nothing is fetched or changed.
    let r = x.run(OpId(6), &download("gross.bin", 10, content(b"")), &mut src);
    assert_eq!(r, LocalResult::Precondition);
    let (files, trash) = s.inventory();
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        vec!["Ziel/gross.bin", "Ziel/leer"]
    );
    assert!(trash.is_empty());
    assert!(s.store.intents().unwrap().is_empty());
}

fn replace_op(v: &View, rel: &str, new: &[u8]) -> LocalOp {
    let (local, e) = &v.obs[&PathBuf::from(rel)];
    LocalOp::Replace {
        local: *local,
        parent: e.parent,
        name: e.name.clone(),
        expect: expect_of(e),
        node: NodeId(42),
        rev: Rev(2),
        content: content(new),
    }
}

fn source(new: &[u8]) -> Source {
    let mut src = Source::default();
    src.0.insert(NodeId(42), new.to_vec());
    src
}

#[test]
fn ersetzen_unveraenderte_datei_gelingt() {
    // The swap changes the original's ctime (on ext4 and APFS): the check after the swap must not
    // compare it, or every replacement would be undone and repeated forever.
    let s = Setup::new();
    fs::write(s.p("plan.txt"), b"alt").unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let r = s.exec(&v, &linked).run(
        OpId(7),
        &replace_op(&v, "plan.txt", b"neu"),
        &mut source(b"neu"),
    );
    let LocalResult::Done { id, fp } = r else {
        panic!("{r:?}")
    };
    assert_ne!(
        id,
        v.obs[&PathBuf::from("plan.txt")].0,
        "a new file took its place"
    );
    assert!(fp.is_some());
    let (files, trash) = s.inventory();
    assert_eq!(files["plan.txt"], b"neu");
    assert_eq!(trash, vec![b"alt".to_vec()]);
    let book = s.store.trash().unwrap();
    assert_eq!(book.len(), 1);
    assert_eq!(book[0].reason, TrashReason::Replaced);
    assert_eq!(book[0].orig_path, b"plan.txt");
    assert!(s.store.intents().unwrap().is_empty());
}

#[test]
fn ersetzen_veraenderte_datei_bleibt() {
    let s = Setup::new();
    fs::write(s.p("plan.txt"), b"alt").unwrap();
    let v = s.view();
    fs::write(s.p("plan.txt"), b"vom Nutzer").unwrap();
    let linked = HashSet::new();
    let r = s.exec(&v, &linked).run(
        OpId(7),
        &replace_op(&v, "plan.txt", b"neu"),
        &mut source(b"neu"),
    );
    assert_eq!(r, LocalResult::Precondition);
    let (files, trash) = s.inventory();
    assert_eq!(files["plan.txt"], b"vom Nutzer");
    assert!(trash.is_empty());
}

/// Writes to a file at one step of the protocol, like a program saving it.
struct Writer {
    at: Step,
    path: PathBuf,
    data: &'static [u8],
    done: Cell<bool>,
}

impl ExecHooks for Writer {
    fn at(&self, step: Step, _target: &Path) {
        if step == self.at && !self.done.get() {
            self.done.set(true);
            fs::write(&self.path, self.data).unwrap();
        }
    }
}

#[test]
fn ersetzen_mit_gleichzeitigem_schreiber_an_jedem_haken() {
    for at in [
        Step::AfterPrecheck,
        Step::ContentWritten,
        Step::IntentWritten,
        Step::AfterSwap,
    ] {
        let s = Setup::new();
        fs::write(s.p("plan.txt"), b"alt").unwrap();
        let v = s.view();
        let linked = HashSet::new();
        let w = Writer {
            at,
            path: s.p("plan.txt"),
            data: b"Speichern des Nutzers",
            done: Cell::new(false),
        };
        let mut x = s.exec(&v, &linked);
        x.hooks = Some(&w);
        let r = x.run(
            OpId(8),
            &replace_op(&v, "plan.txt", b"neu"),
            &mut source(b"neu"),
        );
        assert!(w.done.get());
        let (files, trash) = s.inventory();
        let all: Vec<&Vec<u8>> = files.values().chain(trash.iter()).collect();
        // The user's save is never lost; it is at the name unless it was written into the
        // replacement after the swap (then the replacement's fingerprint is unknown).
        assert!(
            all.contains(&&b"Speichern des Nutzers".to_vec()),
            "{at:?}: {files:?} {trash:?}"
        );
        assert!(
            !files.keys().any(|k| k.contains(".xlrx-dl-")),
            "{at:?}: {files:?}"
        );
        assert!(s.store.intents().unwrap().is_empty(), "{at:?}");
        match at {
            Step::AfterSwap => {
                // The program wrote into the new file: done, but its fingerprint is unknown.
                assert!(
                    matches!(r, LocalResult::Done { fp: None, .. }),
                    "{at:?}: {r:?}"
                );
                assert_eq!(trash, vec![b"alt".to_vec()]);
            }
            _ => {
                assert_eq!(r, LocalResult::Precondition, "{at:?}");
                assert_eq!(files["plan.txt"], b"Speichern des Nutzers", "{at:?}");
                assert!(trash.is_empty(), "{at:?}");
            }
        }
    }
}

#[test]
fn verschieben() {
    let s = Setup::new();
    fs::create_dir(s.p("A")).unwrap();
    fs::create_dir(s.p("B")).unwrap();
    fs::write(s.p("A/x.txt"), b"x").unwrap();
    fs::write(s.p("B/belegt.txt"), b"b").unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let x = s.exec(&v, &linked);
    let (local, e) = v.obs[&PathBuf::from("A/x.txt")].clone();
    let b = v.obs[&PathBuf::from("B")].0;
    let mv = |to: &str, from_name: &str| LocalOp::Move {
        local,
        from_parent: e.parent,
        from_name: name(from_name),
        parent: b,
        name: name(to),
        synced_to: None,
    };
    // The name is taken: nothing happens.
    assert_eq!(
        x.run(OpId(1), &mv("belegt.txt", "x.txt"), &mut Source::default()),
        LocalResult::Precondition
    );
    // Not where the engine thinks it is.
    assert_eq!(
        x.run(OpId(2), &mv("y.txt", "anders.txt"), &mut Source::default()),
        LocalResult::Precondition
    );
    let r = x.run(OpId(3), &mv("y.txt", "x.txt"), &mut Source::default());
    assert!(
        matches!(r, LocalResult::Done { id, fp: Some(_) } if id == local),
        "{r:?}"
    );
    assert_eq!(fs::read(s.p("B/y.txt")).unwrap(), b"x");
    assert_eq!(fs::read(s.p("B/belegt.txt")).unwrap(), b"b");
}

fn delete_op(v: &View, rel: &str) -> LocalOp {
    let (local, e) = &v.obs[&PathBuf::from(rel)];
    LocalOp::DeleteFile {
        local: *local,
        parent: e.parent,
        name: e.name.clone(),
        expect: expect_of(e),
        node: NodeId(43),
    }
}

#[test]
fn loeschen_in_den_papierkorb() {
    let s = Setup::new();
    fs::write(s.p("weg.txt"), b"weg").unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let r = s
        .exec(&v, &linked)
        .run(OpId(11), &delete_op(&v, "weg.txt"), &mut Source::default());
    assert!(matches!(r, LocalResult::Done { .. }), "{r:?}");
    let (files, trash) = s.inventory();
    assert!(files.is_empty());
    assert_eq!(trash, vec![b"weg".to_vec()]);
    let book = s.store.trash().unwrap();
    assert_eq!(
        (book[0].reason, book[0].node, book[0].orig_path.as_slice()),
        (TrashReason::Delete, Some(43), b"weg.txt".as_slice())
    );
}

#[test]
fn loeschen_mit_schreiber_im_papierkorb() {
    let s = Setup::new();
    fs::write(s.p("weg.txt"), b"weg").unwrap();
    let v = s.view();
    let linked = HashSet::new();
    // The program writes after the check, right before the move into the trash: the check in
    // the trash sees it, and the file comes back.
    let w = Writer {
        at: Step::IntentWritten,
        path: s.p("weg.txt"),
        data: b"doch noch gebraucht",
        done: Cell::new(false),
    };
    let mut x = s.exec(&v, &linked);
    x.hooks = Some(&w);
    let r = x.run(OpId(12), &delete_op(&v, "weg.txt"), &mut Source::default());
    assert_eq!(r, LocalResult::Precondition);
    let (files, trash) = s.inventory();
    assert_eq!(files["weg.txt"], b"doch noch gebraucht");
    assert!(trash.is_empty());
    assert!(s.store.intents().unwrap().is_empty());
}

#[test]
fn ordner_loeschen_mit_ds_store() {
    let s = Setup::new();
    fs::create_dir(s.p("leer")).unwrap();
    fs::write(s.p("leer/.DS_Store"), b"finder").unwrap();
    // Two junk files in one tick: each gets its own trash name.
    fs::write(s.p("leer/._foto.jpg"), b"apple").unwrap();
    fs::create_dir(s.p("voll")).unwrap();
    fs::write(s.p("voll/wichtig.txt"), b"w").unwrap();
    fs::create_dir(s.p("voll/.Trashes")).unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let x = s.exec(&v, &linked);
    let rmdir = |rel: &str| {
        let (local, e) = &v.obs[&PathBuf::from(rel)];
        LocalOp::DeleteDir {
            local: *local,
            parent: e.parent,
            name: e.name.clone(),
            node: NodeId(50),
        }
    };
    let r = x.run(OpId(20), &rmdir("leer"), &mut Source::default());
    assert!(matches!(r, LocalResult::Done { .. }), "{r:?}");
    assert!(!s.p("leer").exists());
    // A folder with something else in it stays; a directory never counts as junk.
    assert_eq!(
        x.run(OpId(21), &rmdir("voll"), &mut Source::default()),
        LocalResult::Precondition
    );
    assert!(s.p("voll/wichtig.txt").exists() && s.p("voll/.Trashes").is_dir());
    let (_, mut trash) = s.inventory();
    trash.sort();
    assert_eq!(trash, vec![b"apple".to_vec(), b"finder".to_vec()]);
    let book = s.store.trash().unwrap();
    assert_eq!(book.len(), 2);
    assert!(book.iter().all(|t| t.reason == TrashReason::Junk));
}

#[test]
fn liegengebliebene_download_temps_in_den_papierkorb() {
    let s = Setup::new();
    fs::create_dir(s.p("a")).unwrap();
    fs::write(s.p("a/.xlrx-dl-7-00000000beef"), b"halb").unwrap();
    fs::write(s.p(".xlrx-dl-8-00000000cafe"), b"verknuepft").unwrap();
    fs::write(s.p(".xlrx-dl-9-0000000f00d0"), b"laeuft").unwrap();
    fs::write(s.p("echt.txt"), b"e").unwrap();
    let v = s.view();
    // A linked file renamed to a temporary name, and one an intent still explains: both stay.
    let linked = HashSet::from([v.obs[&PathBuf::from(".xlrx-dl-8-00000000cafe")].0]);
    s.store
        .add_intent(&Intent {
            id: 0,
            op_id: 9,
            kind: IntentKind::Download,
            dir_path: Vec::new(),
            dir_ino: 0,
            target_name: b"x".to_vec(),
            temp_name: Some(b".xlrx-dl-9-0000000f00d0".to_vec()),
            temp_ino: None,
            orig_ino: None,
            trash_name: None,
            expect: None,
            phase: Phase::Writing,
            created_ms: 0,
        })
        .unwrap();
    let x = s.exec(&v, &linked);
    assert_eq!(x.sweep_temps(), Ok(1));
    let (files, trash) = s.inventory();
    assert_eq!(
        files.keys().collect::<Vec<_>>(),
        vec![
            ".xlrx-dl-8-00000000cafe",
            ".xlrx-dl-9-0000000f00d0",
            "echt.txt"
        ]
    );
    // Never removed: kept in the trash with where it lay.
    assert_eq!(trash, vec![b"halb".to_vec()]);
    let book = s.store.trash().unwrap();
    assert_eq!(book[0].reason, TrashReason::Recovered);
    assert_eq!(book[0].orig_path, b"a/.xlrx-dl-7-00000000beef");
    // Already gone: the old index finds nothing to move.
    assert_eq!(x.sweep_temps(), Ok(0));
}

/// A full disk while downloading leaves nothing behind (no half file keeps the disk full, no
/// intent). Needs a small volume: `XLRX_TEST_SMALL_FS` names a writable directory on one with
/// a few MiB (CI: a 4 MiB tmpfs); without it the test checks nothing.
#[test]
fn enospc_beim_download() {
    let Some(base) = std::env::var_os("XLRX_TEST_SMALL_FS") else {
        eprintln!("XLRX_TEST_SMALL_FS nicht gesetzt: übersprungen");
        return;
    };
    let (s, _small) = Setup::below(Path::new(&base));
    fs::create_dir(s.p("Ziel")).unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let x = s.exec(&v, &linked);
    let dir = v.obs[&PathBuf::from("Ziel")].0;
    let big: Vec<u8> = (0..16usize << 20).map(|i| (i * 7 % 251) as u8).collect();
    let mut src = Source::default();
    src.0.insert(NodeId(9), big.clone());
    src.0.insert(NodeId(10), b"klein".to_vec());
    let download = |n: &str, node: u64, c: FileContent| LocalOp::Download {
        parent: dir,
        name: name(n),
        node: NodeId(node),
        node_parent: NodeId(1),
        rev: Rev(1),
        content: c,
    };
    let (r, why) = x.run_explained(OpId(3), &download("gross.bin", 9, content(&big)), &mut src);
    assert_eq!(r, LocalResult::Error);
    let why = why.unwrap();
    assert!(why.contains("(os error 28)"), "{why}");
    let (files, trash) = s.inventory();
    assert!(files.is_empty() && trash.is_empty(), "{files:?}");
    assert!(s.store.intents().unwrap().is_empty());
    // The space is free again: a small file fits.
    let r = x.run(
        OpId(4),
        &download("klein.txt", 10, content(b"klein")),
        &mut src,
    );
    assert!(matches!(r, LocalResult::Done { .. }), "{r:?}");
    assert_eq!(fs::read(s.p("Ziel/klein.txt")).unwrap(), b"klein");
}

/// Dies (panics) at one step of the protocol.
struct Die(Step);

impl ExecHooks for Die {
    fn at(&self, step: Step, _target: &Path) {
        if step == self.0 {
            panic!("Absturz bei {step:?}");
        }
    }
}

#[test]
fn abbruch_an_jedem_haken_dann_wiederherstellung() {
    // The process dies at every step of every protocol; afterwards `recover()` runs. Oracle:
    // every version that existed (the original, the new content) is at its place, in the trash
    // or rescued next to it; nothing temporary is left; no intent is left.
    let cases: [(Step, &str); 9] = [
        (Step::AfterPrecheck, "replace"),
        (Step::ContentWritten, "replace"),
        (Step::IntentWritten, "replace"),
        (Step::AfterSwap, "replace"),
        (Step::AfterTrash, "replace"),
        (Step::IntentWritten, "delete"),
        (Step::AfterTrash, "delete"),
        (Step::BeforeFinalRename, "download"),
        (Step::AfterFinalRename, "download"),
    ];
    for (step, kind) in cases {
        let s = Setup::new();
        fs::write(s.p("plan.txt"), b"alt").unwrap();
        let v = s.view();
        let linked = HashSet::new();
        let die = Die(step);
        let mut x = s.exec(&v, &linked);
        x.hooks = Some(&die);
        let op = match kind {
            "replace" => replace_op(&v, "plan.txt", b"neu"),
            "delete" => delete_op(&v, "plan.txt"),
            _ => LocalOp::Download {
                parent: root_id(&v),
                name: name("neu.txt"),
                node: NodeId(42),
                node_parent: NodeId(1),
                rev: Rev(1),
                content: content(b"neu"),
            },
        };
        let src = RefCell::new(source(b"neu"));
        let died = catch_unwind(AssertUnwindSafe(|| {
            x.run(OpId(30), &op, &mut *src.borrow_mut())
        }))
        .is_err();
        assert!(died, "{step:?} {kind}");
        let x = s.exec(&v, &linked);
        let rec = x.recover().unwrap();
        let (files, trash) = s.inventory();
        let label = format!("{step:?} {kind}: {rec:?} {files:?} {trash:?}");
        assert!(s.store.intents().unwrap().is_empty(), "{label}");
        assert!(!files.keys().any(|k| k.contains(".xlrx-dl-")), "{label}");
        let all: Vec<&Vec<u8>> = files.values().chain(trash.iter()).collect();
        assert!(all.contains(&&b"alt".to_vec()), "original lost: {label}");
        // Every file in the trash is in the book.
        assert_eq!(s.store.trash().unwrap().len(), trash.len(), "{label}");
        if kind != "delete" {
            // Nothing exists twice at a visible place.
            let visible_alt = files.values().filter(|f| f.as_slice() == b"alt").count();
            assert!(visible_alt <= 1, "{label}");
        }
    }
}

/// Puts a foreign file under the client's temporary name and changes the original, at one step.
struct Intruder {
    dir: PathBuf,
    original: PathBuf,
    done: Cell<bool>,
}

impl ExecHooks for Intruder {
    fn at(&self, step: Step, _target: &Path) {
        if step == Step::ContentWritten && !self.done.get() {
            self.done.set(true);
            let temp = fs::read_dir(&self.dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| {
                    p.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with(".xlrx-dl-")
                })
                .unwrap();
            fs::write(self.dir.join("fremd"), b"fremde Daten").unwrap();
            fs::rename(self.dir.join("fremd"), &temp).unwrap();
            fs::write(&self.original, b"auch geaendert").unwrap();
        }
    }
}

#[test]
fn fremde_datei_unter_temp_namen_wird_nie_entfernt() {
    let s = Setup::new();
    fs::write(s.p("plan.txt"), b"alt").unwrap();
    let v = s.view();
    let linked = HashSet::new();
    let hook = Intruder {
        dir: s.root(),
        original: s.p("plan.txt"),
        done: Cell::new(false),
    };
    let mut x = s.exec(&v, &linked);
    x.hooks = Some(&hook);
    let r = x.run(
        OpId(40),
        &replace_op(&v, "plan.txt", b"neu"),
        &mut source(b"neu"),
    );
    assert!(hook.done.get());
    assert_eq!(r, LocalResult::Error);
    let (files, _) = s.inventory();
    assert_eq!(files["plan.txt"], b"auch geaendert");
    // Not proven to be the client's own: kept.
    assert!(
        files
            .iter()
            .any(|(k, v)| k.starts_with(".xlrx-dl-") && v.as_slice() == b"fremde Daten"),
        "{files:?}"
    );
    // Its intent is still open, so the sweep leaves it to `recover`, which keeps it in the trash.
    let v = s.view();
    let x = s.exec(&v, &linked);
    assert_eq!(x.sweep_temps(), Ok(0));
    assert_eq!(x.recover().unwrap().trashed, 1);
    let (files, trash) = s.inventory();
    assert!(
        !files.keys().any(|k| k.starts_with(".xlrx-dl-")),
        "{files:?}"
    );
    assert!(trash.contains(&b"fremde Daten".to_vec()));
}

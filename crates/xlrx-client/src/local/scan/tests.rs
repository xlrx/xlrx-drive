use std::cell::Cell;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;

use super::*;

/// A clock far ahead of every file's timestamps: quiet periods are over, nothing is recent.
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

struct Setup {
    t: tempfile::TempDir,
    store: LocalStore,
}

impl Setup {
    fn new() -> Self {
        let t = tempfile::tempdir().unwrap();
        let store = LocalStore::open(&t.path().join("local.sqlite")).unwrap();
        fs::create_dir(t.path().join("root")).unwrap();
        Self { t, store }
    }

    fn root(&self) -> PathBuf {
        self.t.path().join("root")
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root().join(rel)
    }

    fn scan(&self, linked: &HashSet<LocalId>, clock: &dyn Clock) -> Snapshot {
        full_scan(
            &self.root(),
            &self.store,
            linked,
            ScanSettings::default(),
            clock,
            None,
        )
        .unwrap()
    }

    fn id(&self, rel: &str) -> LocalId {
        let parent = self.p(rel).parent().unwrap().to_owned();
        let d = Dir::open(&parent).unwrap();
        local_id(&d.stat(self.p(rel).file_name().unwrap()).unwrap())
    }
}

fn names(s: &Snapshot) -> BTreeMap<PathBuf, (Kind, bool)> {
    s.obs
        .iter()
        .map(|o| {
            (
                s.index.path(o.id).unwrap(),
                (o.entry.kind, o.entry.content.is_some()),
            )
        })
        .collect()
}

fn opaque(s: &Snapshot) -> BTreeMap<PathBuf, OpaqueWhy> {
    s.opaque.iter().map(|o| (o.path.clone(), o.why)).collect()
}

#[test]
fn ordner_und_dateien_mit_inhalt() {
    let s = Setup::new();
    fs::create_dir_all(s.p("Projekte/2026")).unwrap();
    fs::write(s.p("Projekte/2026/plan.txt"), b"Plan").unwrap();
    fs::write(s.p("leer"), b"").unwrap();
    let snap = s.scan(&HashSet::new(), &Later::new());
    assert!(snap.opaque.is_empty(), "{:?}", snap.opaque);
    assert!(snap.stable);
    assert!(snap.blind.is_empty());
    assert_eq!(
        names(&snap),
        BTreeMap::from([
            (PathBuf::from("Projekte"), (Kind::Dir, false)),
            (PathBuf::from("Projekte/2026"), (Kind::Dir, false)),
            (PathBuf::from("Projekte/2026/plan.txt"), (Kind::File, true)),
            (PathBuf::from("leer"), (Kind::File, true)),
        ])
    );
    let plan = snap
        .obs
        .iter()
        .find(|o| o.entry.name.as_str() == "plan.txt")
        .unwrap();
    assert_eq!(
        plan.entry.content,
        Some(Chunker::new().digest_slice(b"Plan").content)
    );
    assert_eq!(plan.entry.parent, s.id("Projekte/2026"));
    assert_eq!(
        snap.root,
        local_id(&Dir::open(&s.root()).unwrap().meta().unwrap())
    );
    // The hash went into the cache.
    let fp = plan.entry.fp.unwrap();
    assert_eq!(s.store.cached(plan.id, fp).unwrap(), plan.entry.content);
}

#[test]
fn ruhefrist_dann_inhalt() {
    let s = Setup::new();
    fs::write(s.p("frisch.txt"), b"x").unwrap();
    fs::write(s.p("daten.sqlite"), b"y").unwrap();
    let now = SystemClock.now_ns();
    let clock = Later(Cell::new(now));
    let snap = s.scan(&HashSet::new(), &clock);
    assert!(snap.obs.iter().all(|o| o.entry.content.is_none()));
    let retry = snap.retry_at_ns.unwrap();
    assert!(retry > now && retry <= now + 4_000_000_000);
    // After the quiet period of ordinary files, databases still wait.
    let clock = Later(Cell::new(now + 5_000_000_000));
    let snap = s.scan(&HashSet::new(), &clock);
    let got = names(&snap);
    assert_eq!(got[&PathBuf::from("frisch.txt")], (Kind::File, true));
    assert_eq!(got[&PathBuf::from("daten.sqlite")], (Kind::File, false));
}

#[test]
fn eigener_ordner_und_junk_werden_weggelassen() {
    let s = Setup::new();
    fs::create_dir_all(s.p(".xlrx-client/trash")).unwrap();
    fs::write(s.p(".xlrx-client/trash/x"), b"x").unwrap();
    fs::write(s.p(".DS_Store"), b"junk").unwrap();
    fs::write(s.p("~$Bericht.docx"), b"lock").unwrap();
    fs::write(s.p(".xlrx-dl-7-ab"), b"halb").unwrap();
    fs::write(s.p("Bericht.docx"), b"B").unwrap();
    let snap = s.scan(&HashSet::new(), &Later::new());
    assert!(snap.opaque.is_empty(), "{:?}", snap.opaque);
    let got: Vec<_> = names(&snap).into_keys().collect();
    // Download temporaries are reported: the engine knows them and never uploads them.
    assert_eq!(
        got,
        vec![
            PathBuf::from(".xlrx-dl-7-ab"),
            PathBuf::from("Bericht.docx")
        ]
    );
}

#[test]
fn verknuepfte_datei_mit_ignoriertem_namen_ist_opak() {
    let s = Setup::new();
    fs::write(s.p("~$Bericht.docx"), b"umbenannt").unwrap();
    let linked = HashSet::from([s.id("~$Bericht.docx")]);
    let snap = s.scan(&linked, &Later::new());
    assert!(snap.obs.is_empty());
    assert_eq!(
        opaque(&snap),
        BTreeMap::from([(PathBuf::from("~$Bericht.docx"), OpaqueWhy::IgnoredName)])
    );
}

#[test]
fn ignorierte_und_ungueltige_ordner_werden_durchlaufen() {
    let s = Setup::new();
    // A folder renamed to a name the server ignores, with a file inside.
    fs::create_dir_all(s.p("#recycle/Altlasten")).unwrap();
    fs::write(s.p("#recycle/Altlasten/a.pdf"), b"a").unwrap();
    // A folder whose name is not UTF-8 (an old archive on Linux; APFS refuses such names).
    let bad = s.root().join(OsStr::from_bytes(b"M\x81ller"));
    let utf8_only = fs::create_dir(&bad).is_err();
    if !utf8_only {
        fs::write(bad.join("Lebenslauf.pdf"), b"L").unwrap();
    }
    let snap = s.scan(&HashSet::new(), &Later::new());
    assert!(snap.obs.is_empty(), "{:?}", names(&snap));
    let got = opaque(&snap);
    assert_eq!(got[&PathBuf::from("#recycle")], OpaqueWhy::IgnoredName);
    assert_eq!(
        got[&PathBuf::from("#recycle/Altlasten")],
        OpaqueWhy::InsideOpaque
    );
    assert_eq!(
        got[&PathBuf::from("#recycle/Altlasten/a.pdf")],
        OpaqueWhy::InsideOpaque
    );
    if utf8_only {
        assert_eq!(got.len(), 3, "{got:?}");
        return;
    }
    let bad_rel = PathBuf::from(OsStr::from_bytes(b"M\x81ller"));
    assert_eq!(got[&bad_rel], OpaqueWhy::NameNotSyncable);
    assert_eq!(
        got[&bad_rel.join("Lebenslauf.pdf")],
        OpaqueWhy::InsideOpaque
    );
}

#[test]
fn links_sonderdateien_harte_links_und_nfc_zwillinge() {
    let s = Setup::new();
    fs::write(s.p("ziel.txt"), b"z").unwrap();
    std::os::unix::fs::symlink("ziel.txt", s.p("link")).unwrap();
    fs::create_dir(s.p("ordner")).unwrap();
    std::os::unix::fs::symlink(s.p("ordner"), s.p("ordnerlink")).unwrap();
    // A named pipe (by the tool: rustix has no `mknodat` on Apple).
    assert!(
        std::process::Command::new("mkfifo")
            .arg(s.p("rohr"))
            .status()
            .unwrap()
            .success()
    );
    fs::write(s.p("hart1"), b"h").unwrap();
    fs::hard_link(s.p("hart1"), s.p("ordner/hart2")).unwrap();
    fs::write(s.p("\u{00C4}pfel"), b"nfc").unwrap();
    // On APFS both spellings are one name: the second write replaces the first, no twins.
    fs::write(s.p("A\u{0308}pfel"), b"nfd").unwrap();
    let one_name = fs::read_dir(s.root()).unwrap().count() == 7;
    let snap = s.scan(&HashSet::new(), &Later::new());
    let got = opaque(&snap);
    assert_eq!(got[&PathBuf::from("link")], OpaqueWhy::Symlink);
    assert_eq!(got[&PathBuf::from("ordnerlink")], OpaqueWhy::Symlink);
    assert_eq!(got[&PathBuf::from("rohr")], OpaqueWhy::Special);
    // Both names of the hard link are one identity: opaque, recorded once.
    let hard: Vec<_> = snap
        .opaque
        .iter()
        .filter(|o| matches!(o.why, OpaqueWhy::HardLinked | OpaqueWhy::DuplicateId))
        .collect();
    assert_eq!(hard.len(), 1, "{:?}", snap.opaque);
    let twins = snap
        .opaque
        .iter()
        .filter(|o| o.why == OpaqueWhy::NfcTwin)
        .count();
    let mut ok: Vec<_> = names(&snap).into_keys().collect();
    if one_name {
        assert_eq!(twins, 0, "{:?}", snap.opaque);
        assert_eq!(ok.len(), 3, "{ok:?}");
        ok.retain(|p| p.to_str().is_some_and(|n| n.is_ascii()));
    } else {
        assert_eq!(twins, 2, "{:?}", snap.opaque);
    }
    assert_eq!(ok, vec![PathBuf::from("ordner"), PathBuf::from("ziel.txt")]);
}

#[test]
fn unlesbarer_ordner_ist_blind() {
    let s = Setup::new();
    fs::create_dir(s.p("geheim")).unwrap();
    fs::write(s.p("geheim/a"), b"a").unwrap();
    fs::set_permissions(s.p("geheim"), fs::Permissions::from_mode(0o000)).unwrap();
    // As root, permissions do not apply: nothing to test then (CI runs as a normal user).
    if fs::read_dir(s.p("geheim")).is_ok() {
        fs::set_permissions(s.p("geheim"), fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let snap = s.scan(&HashSet::new(), &Later::new());
    fs::set_permissions(s.p("geheim"), fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(snap.blind, vec![s.id("geheim")]);
    assert_eq!(
        opaque(&snap),
        BTreeMap::from([(PathBuf::from("geheim"), OpaqueWhy::NoAccess)])
    );
}

#[test]
fn wurzel_fehlt_bricht_ab() {
    let s = Setup::new();
    fs::remove_dir(s.root()).unwrap();
    let r = full_scan(
        &s.root(),
        &s.store,
        &HashSet::new(),
        ScanSettings::default(),
        &Later::new(),
        None,
    );
    assert!(matches!(r, Err(ScanAbort::Root(_))));
}

/// Moves a folder from a directory not read yet into one already read, once.
struct MoveDuringScan {
    root: PathBuf,
    after: PathBuf,
    from: &'static str,
    to: &'static str,
    done: Cell<bool>,
}

impl ScanHooks for MoveDuringScan {
    fn dir_read(&self, rel: &Path) {
        if rel == self.after && !self.done.get() {
            self.done.set(true);
            fs::rename(self.root.join(self.from), self.root.join(self.to)).unwrap();
        }
    }
}

#[test]
fn verschieben_waehrend_vollem_scan() {
    // Both directions: into a directory already read, and out of one already read.
    for (from, to) in [("A/Projekt", "B/Projekt"), ("B/Projekt", "A/Projekt")] {
        let s = Setup::new();
        fs::create_dir_all(s.p("A")).unwrap();
        fs::create_dir_all(s.p("B")).unwrap();
        fs::create_dir_all(s.p(&format!("{from}/Unterordner"))).unwrap();
        for i in 0..5 {
            fs::write(s.p(&format!("{from}/Unterordner/{i}.txt")), b"x").unwrap();
        }
        let projekt = s.id(from);
        let parent_to = s.id(to.split('/').next().unwrap());
        // After the first of A and B was read, whichever it is.
        let first = Dir::open(&s.root())
            .unwrap()
            .entries()
            .unwrap()
            .into_iter()
            .map(|(n, _)| n)
            .find(|n| n == "A" || n == "B")
            .unwrap();
        let hook = MoveDuringScan {
            root: s.root(),
            after: PathBuf::from(first),
            from,
            to,
            done: Cell::new(false),
        };
        let snap = full_scan(
            &s.root(),
            &s.store,
            &HashSet::new(),
            ScanSettings::default(),
            &SystemClock,
            Some(&hook),
        )
        .unwrap();
        assert!(hook.done.get());
        // The moved folder and everything in it are in the snapshot, at the new place only.
        let got = names(&snap);
        assert!(got.contains_key(&PathBuf::from(to)), "{got:?}");
        assert_eq!(
            got.keys()
                .filter(|p| p.starts_with(format!("{to}/Unterordner")))
                .count(),
            6,
            "{got:?}"
        );
        assert!(!got.keys().any(|p| p.starts_with(from)), "{got:?}");
        assert_eq!(
            snap.obs
                .iter()
                .find(|o| o.id == projekt)
                .unwrap()
                .entry
                .parent,
            parent_to
        );
        assert!(snap.stable, "a pass after the re-read changed nothing");
    }
}

//! Setting up a sync folder and checking it at every start (ADR 0002 §7.2): which volumes are
//! allowed, what they do with names and timestamps, and the marker that proves the folder is
//! still the same one.

use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use rand::Rng as _;
use xlrx_chunk::RACY_WINDOW_NS;
use xlrx_fs::{Dir, FsType, RenameMode};
use xlrx_proto::Name;
use xlrx_proto::name::CLIENT_DIR;

/// What the volume of a sync folder does, as measured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeCaps {
    pub fs: FsType,
    /// "Probe-Aa" and "probe-aA" are the same name (APFS default).
    pub case_insensitive: bool,
    /// NFC and NFD spellings are the same name (APFS).
    pub normalization_insensitive: bool,
    /// The volume records a birth time (Linux identities use it).
    pub btime: bool,
    /// Resolution of modification times.
    pub granularity_ns: i64,
    /// Unsafe window: a file changed this recently is rehashed before it is trusted.
    pub window_ns: i64,
}

/// Why a folder cannot be synced.
#[derive(Debug)]
pub enum Refusal {
    /// Not a file system with atomic swap, no-replace rename and stable inodes.
    FileSystem(FsType),
    /// Synology Drive syncs this folder: two sync clients on one folder destroy each other's
    /// work (PLAN 4.1).
    SynologyDrive,
    /// Synced by iCloud (or another File Provider).
    Cloud,
    /// The root of a volume (its `.Trashes`, `.fseventsd` … would be synced).
    VolumeRoot,
    /// The volume treats two names as one that the client's folding keeps apart.
    Folding(String, String),
    Io(io::Error),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FileSystem(t) => write!(f, "Dateisystem {t:?} wird nicht unterstützt"),
            Self::SynologyDrive => write!(f, "Ordner wird von Synology Drive synchronisiert"),
            Self::Cloud => write!(
                f,
                "Ordner liegt in iCloud oder einem anderen Cloud-Speicher"
            ),
            Self::VolumeRoot => write!(
                f,
                "Die Wurzel eines Laufwerks kann nicht synchronisiert werden"
            ),
            Self::Folding(a, b) => write!(
                f,
                "Das Laufwerk behandelt „{a}“ und „{b}“ als denselben Namen"
            ),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Refusal {}

impl From<io::Error> for Refusal {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// Pairs some volumes merge (full case folding); the client's folding must merge them too.
const FOLDING_PAIRS: &[(&str, &str)] = &[
    ("ß", "SS"),
    ("ẞ", "ss"),
    ("ς", "σ"),
    ("ﬁ", "FI"),
    ("A\u{0308}", "ä"),
];

fn exists(d: &Dir, name: &str) -> io::Result<bool> {
    match d.stat(OsStr::new(name)) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Checks whether a folder can be synced and measures its volume. Writes only below
/// `<root>/.xlrx-client/probe/` and removes what it wrote.
pub fn probe(root: &Path) -> Result<VolumeCaps, Refusal> {
    let dir = Dir::open(root)?;
    let fs = dir.fs_type()?;
    if !fs.supported() {
        return Err(Refusal::FileSystem(fs));
    }
    if exists(&dir, ".SynologyWorkingDirectory")? {
        return Err(Refusal::SynologyDrive);
    }
    let text = root.to_string_lossy();
    if text.contains("/Library/Mobile Documents") || text.contains("/Library/CloudStorage") {
        return Err(Refusal::Cloud);
    }
    let parent = root.parent().ok_or(Refusal::VolumeRoot)?;
    if Dir::open(parent)?.meta()?.dev != dir.meta()?.dev {
        return Err(Refusal::VolumeRoot);
    }
    let client = ensure_dir(&dir, CLIENT_DIR)?;
    let probe_dir = ensure_dir(&client, "probe")?;
    let caps = measure(&probe_dir, fs);
    // The probe's own files: created a moment ago under unique names.
    for (name, _) in probe_dir.entries()? {
        probe_dir.unlink(&name)?;
    }
    client.rmdir(OsStr::new("probe"))?;
    caps
}

fn ensure_dir(dir: &Dir, name: &str) -> io::Result<Dir> {
    match dir.mkdir(OsStr::new(name)) {
        Ok(()) => dir.sync()?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    dir.open_dir(OsStr::new(name))
}

fn measure(d: &Dir, fs: FsType) -> Result<VolumeCaps, Refusal> {
    let create = |n: &str| d.create_new(OsStr::new(n)).map(drop);
    create("Probe-Aa")?;
    let case_insensitive = exists(d, "probe-aA")?;
    create("Probe-\u{00C4}")?;
    let normalization_insensitive = exists(d, "Probe-A\u{0308}")?;
    if case_insensitive || normalization_insensitive {
        for (i, (a, b)) in FOLDING_PAIRS.iter().enumerate() {
            let (na, nb) = (format!("p{i}-{a}"), format!("p{i}-{b}"));
            create(&na)?;
            let merged = exists(d, &nb)?;
            let key = |s: &str| Name::new(s).map(|n| n.local_fold_key()).ok();
            if merged && key(&na) != key(&nb) {
                return Err(Refusal::Folding(a.to_string(), b.to_string()));
            }
        }
    }
    // Timestamp resolution: set a time with odd nanoseconds and read it back.
    create("zeit")?;
    let odd = 1_759_500_000_123_456_789;
    d.set_mtime(OsStr::new("zeit"), odd)?;
    let meta = d.stat(OsStr::new("zeit"))?;
    let lost = (odd - meta.mtime_ns).abs();
    let mut granularity_ns = 1;
    while granularity_ns <= lost && granularity_ns < 2_000_000_000 {
        granularity_ns *= 10;
    }
    Ok(VolumeCaps {
        fs,
        case_insensitive,
        normalization_insensitive,
        btime: meta.btime_ns.is_some(),
        granularity_ns,
        window_ns: RACY_WINDOW_NS.max(2 * granularity_ns),
    })
}

/// The marker in `<root>/.xlrx-client/root.json`: proves at every start that the folder is
/// still the one that was set up (not unmounted, replaced or moved; `st_dev` is not stable on
/// macOS for external volumes, so it is not used).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    pub folder: String,
    pub token: String,
    pub root_ino: u64,
}

const MARKER: &str = "root.json";

fn random_hex(bytes: usize) -> String {
    let mut b = vec![0u8; bytes];
    rand::rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Writes a new marker for folder `folder` (durably: temporary file, sync, no-replace rename).
pub fn create_marker(root: &Path, folder: &str) -> Result<Marker, Refusal> {
    let dir = Dir::open(root)?;
    let client = ensure_dir(&dir, CLIENT_DIR)?;
    let marker = Marker {
        folder: folder.to_owned(),
        token: random_hex(16),
        root_ino: dir.meta()?.ino,
    };
    let json = format!(
        "{{\"folder\":{},\"token\":\"{}\"}}\n",
        escape(&marker.folder),
        marker.token
    );
    let temp = format!("{MARKER}.{}", random_hex(4));
    let mut f = client.create_new(OsStr::new(&temp))?;
    f.write_all(json.as_bytes())?;
    xlrx_fs::sync_file(&f)?;
    client.rename(
        OsStr::new(&temp),
        &client,
        OsStr::new(MARKER),
        RenameMode::NoReplace,
    )?;
    client.sync()?;
    Ok(marker)
}

/// JSON string literal (the folder id is a UUID; escaped anyway).
fn escape(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Why the folder is not the one that was set up.
#[derive(Debug)]
pub enum RootProblem {
    /// Missing, unreadable or not a directory (unmounted volume, deleted folder).
    Gone(io::Error),
    /// Another folder (moved, recreated, other volume mounted in its place).
    Replaced,
}

/// Reads the marker and checks it against the one written at setup.
pub fn check_marker(root: &Path, expected: &Marker) -> Result<(), RootProblem> {
    let dir = Dir::open(root).map_err(RootProblem::Gone)?;
    let ino = dir.meta().map_err(RootProblem::Gone)?.ino;
    let mut text = String::new();
    dir.open_dir(OsStr::new(CLIENT_DIR))
        .and_then(|c| c.open_file(OsStr::new(MARKER)))
        .and_then(|mut f| f.read_to_string(&mut text))
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                RootProblem::Replaced
            } else {
                RootProblem::Gone(e)
            }
        })?;
    let token_ok = text.contains(&format!("\"token\":\"{}\"", expected.token));
    let folder_ok = text.contains(&format!("\"folder\":{}", escape(&expected.folder)));
    if ino == expected.root_ino && token_ok && folder_ok {
        Ok(())
    } else {
        Err(RootProblem::Replaced)
    }
}

/// Where a folder's client directory lies.
pub fn client_dir(root: &Path) -> PathBuf {
    root.join(CLIENT_DIR)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn volume_wird_vermessen() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Drive");
        fs::create_dir(&root).unwrap();
        match probe(&root) {
            Ok(caps) => {
                match caps.fs {
                    // ext4 without casefold: names are exact.
                    FsType::Ext4 => {
                        assert!(!caps.case_insensitive && !caps.normalization_insensitive)
                    }
                    // APFS treats NFC and NFD as one name; case depends on the variant.
                    FsType::Apfs => assert!(caps.normalization_insensitive),
                    _ => {}
                }
                assert_eq!(caps.granularity_ns, 1);
                assert_eq!(caps.window_ns, RACY_WINDOW_NS);
                // The probe leaves nothing behind but the client directory.
                let left: Vec<_> = fs::read_dir(root.join(CLIENT_DIR)).unwrap().collect();
                assert!(left.is_empty());
            }
            // The machine running the tests may use another file system.
            Err(Refusal::FileSystem(_)) => {}
            Err(e) => panic!("{e}"),
        }
    }

    #[test]
    fn synology_ordner_und_volume_wurzel_werden_abgelehnt() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("SynologyDrive");
        fs::create_dir_all(root.join(".SynologyWorkingDirectory")).unwrap();
        assert!(matches!(
            probe(&root),
            Err(Refusal::SynologyDrive | Refusal::FileSystem(_))
        ));
        assert!(matches!(
            probe(Path::new("/")),
            Err(Refusal::VolumeRoot | Refusal::FileSystem(_))
        ));
    }

    #[test]
    fn wurzel_entfernt_ersetzt_ausgehaengt() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("Drive");
        fs::create_dir(&root).unwrap();
        let m = create_marker(&root, "a1b2-\"x\"").unwrap();
        check_marker(&root, &m).unwrap();
        // Moved away and a new folder in its place (with a copied marker): another inode.
        fs::rename(&root, t.path().join("alt")).unwrap();
        fs::create_dir_all(root.join(CLIENT_DIR)).unwrap();
        fs::copy(
            t.path().join("alt").join(CLIENT_DIR).join(MARKER),
            root.join(CLIENT_DIR).join(MARKER),
        )
        .unwrap();
        assert!(matches!(
            check_marker(&root, &m),
            Err(RootProblem::Replaced)
        ));
        // Gone entirely.
        fs::remove_dir_all(&root).unwrap();
        assert!(matches!(check_marker(&root, &m), Err(RootProblem::Gone(_))));
        // A different token at the same place.
        fs::rename(t.path().join("alt"), &root).unwrap();
        let other = Marker {
            token: "0".repeat(32),
            ..m.clone()
        };
        assert!(matches!(
            check_marker(&root, &other),
            Err(RootProblem::Replaced)
        ));
        check_marker(&root, &m).unwrap();
        // Unmounted: the mount point stays as an empty folder (Linux) or vanishes (macOS).
        fs::rename(&root, t.path().join("alt")).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(matches!(
            check_marker(&root, &m),
            Err(RootProblem::Replaced)
        ));
        fs::remove_dir(&root).unwrap();
        // A file in its place.
        fs::write(&root, b"x").unwrap();
        assert!(matches!(check_marker(&root, &m), Err(RootProblem::Gone(_))));
        fs::remove_file(&root).unwrap();
        fs::rename(t.path().join("alt"), &root).unwrap();
        check_marker(&root, &m).unwrap();
    }
}

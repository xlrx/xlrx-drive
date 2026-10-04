use std::io::{Read, Write};

use super::*;

fn os(s: &str) -> &OsStr {
    OsStr::new(s)
}

fn errno(e: &io::Error) -> Option<i32> {
    e.raw_os_error()
}

/// A named pipe (by the tool: rustix has no `mknodat` on Apple).
fn mkfifo(path: &std::path::Path) {
    let ok = std::process::Command::new("mkfifo")
        .arg(path)
        .status()
        .unwrap()
        .success();
    assert!(ok, "mkfifo {}", path.display());
}

fn setup() -> (tempfile::TempDir, Dir) {
    let t = tempfile::tempdir().unwrap();
    let d = Dir::open(t.path()).unwrap();
    (t, d)
}

fn write(d: &Dir, name: &str, data: &[u8]) {
    let mut f = d.create_new(os(name)).unwrap();
    f.write_all(data).unwrap();
    sync_file(&f).unwrap();
}

fn read(d: &Dir, name: &str) -> Vec<u8> {
    let mut v = Vec::new();
    d.open_file(os(name)).unwrap().read_to_end(&mut v).unwrap();
    v
}

#[test]
fn anlegen_lesen_auflisten() {
    let (_t, d) = setup();
    write(&d, "a.txt", b"hallo");
    d.mkdir(os("Ordner")).unwrap();
    let m = d.stat(os("a.txt")).unwrap();
    assert_eq!((m.kind, m.size, m.nlink), (FileKind::File, 5, 1));
    assert_eq!(d.stat(os("Ordner")).unwrap().kind, FileKind::Dir);
    assert_eq!(read(&d, "a.txt"), b"hallo");
    let f = d.open_file(os("a.txt")).unwrap();
    assert_eq!(meta_of(&f).unwrap().ino, m.ino);
    let mut names: Vec<_> = d.entries().unwrap().into_iter().map(|(n, _)| n).collect();
    names.sort();
    assert_eq!(
        names,
        vec![OsString::from("Ordner"), OsString::from("a.txt")]
    );
    let e = d.create_new(os("a.txt")).unwrap_err();
    assert_eq!(errno(&e), Some(rustix::io::Errno::EXIST.raw_os_error()));
    let sub = d.open_dir(os("Ordner")).unwrap();
    assert_eq!(sub.meta().unwrap().ino, d.stat(os("Ordner")).unwrap().ino);
    d.sync().unwrap();
}

#[test]
fn umbenennen_ohne_ueberschreiben_und_tausch() {
    let (_t, d) = setup();
    write(&d, "a", b"A");
    write(&d, "b", b"B");
    let (ia, ib) = (d.stat(os("a")).unwrap().ino, d.stat(os("b")).unwrap().ino);
    // NoReplace refuses an existing target and changes nothing.
    let e = d
        .rename(os("a"), &d, os("b"), RenameMode::NoReplace)
        .unwrap_err();
    assert_eq!(errno(&e), Some(rustix::io::Errno::EXIST.raw_os_error()));
    assert_eq!(read(&d, "b"), b"B");
    // Exchange swaps atomically: the inodes follow the names.
    d.rename(os("a"), &d, os("b"), RenameMode::Exchange)
        .unwrap();
    assert_eq!(read(&d, "a"), b"B");
    assert_eq!(d.stat(os("a")).unwrap().ino, ib);
    assert_eq!(d.stat(os("b")).unwrap().ino, ia);
    // The inode survives a rename into another directory.
    d.mkdir(os("sub")).unwrap();
    let sub = d.open_dir(os("sub")).unwrap();
    d.rename(os("a"), &sub, os("x"), RenameMode::NoReplace)
        .unwrap();
    assert_eq!(sub.stat(os("x")).unwrap().ino, ib);
    // Plain replaces.
    write(&d, "c", b"C");
    d.rename(os("c"), &d, os("b"), RenameMode::Plain).unwrap();
    assert_eq!(read(&d, "b"), b"C");
    assert!(d.stat(os("c")).is_err());
}

#[test]
fn keine_symbolischen_links_folgen() {
    let (t, d) = setup();
    write(&d, "ziel", b"Z");
    d.mkdir(os("zielordner")).unwrap();
    std::os::unix::fs::symlink(t.path().join("ziel"), t.path().join("link")).unwrap();
    std::os::unix::fs::symlink(t.path().join("zielordner"), t.path().join("ordnerlink")).unwrap();
    assert_eq!(d.stat(os("link")).unwrap().kind, FileKind::Symlink);
    let e = d.open_file(os("link")).unwrap_err();
    assert_eq!(errno(&e), Some(rustix::io::Errno::LOOP.raw_os_error()));
    assert!(d.open_dir(os("ordnerlink")).is_err());
    assert!(Dir::open(&t.path().join("ordnerlink")).is_err());
    let e = d.create_new(os("link")).unwrap_err();
    assert_eq!(errno(&e), Some(rustix::io::Errno::EXIST.raw_os_error()));
}

#[test]
fn fifo_blockiert_nicht() {
    let (t, d) = setup();
    mkfifo(&t.path().join("rohr"));
    assert_eq!(d.stat(os("rohr")).unwrap().kind, FileKind::Other);
    // Opening returns at once (no writer needed); the kind tells the caller to skip it.
    let f = d.open_file(os("rohr")).unwrap();
    assert_eq!(meta_of(&f).unwrap().kind, FileKind::Other);
}

#[test]
fn ordner_nur_leer_entfernen() {
    let (_t, d) = setup();
    d.mkdir(os("voll")).unwrap();
    let voll = d.open_dir(os("voll")).unwrap();
    write(&voll, "x", b"x");
    let e = d.rmdir(os("voll")).unwrap_err();
    assert_eq!(errno(&e), Some(rustix::io::Errno::NOTEMPTY.raw_os_error()));
    voll.unlink(os("x")).unwrap();
    d.rmdir(os("voll")).unwrap();
    assert!(d.stat(os("voll")).is_err());
}

#[test]
fn zeitstempel_genau() {
    let (_t, d) = setup();
    write(&d, "a", b"a");
    let ns = 1_759_500_000_123_456_789;
    d.set_mtime(os("a"), ns).unwrap();
    let m = d.stat(os("a")).unwrap();
    // ext4, Btrfs, XFS, tmpfs and APFS keep nanoseconds.
    assert_eq!(m.mtime_ns, ns);
    assert!(m.ctime_ns > 0);
}

#[test]
fn dateisystem_erkennen() {
    let (_t, d) = setup();
    let t = d.fs_type().unwrap();
    // Whatever the test machine uses: known types are supported, the rest is named.
    if let FsType::Other(name) = &t {
        assert!(!name.is_empty());
        assert!(!t.supported());
    } else {
        assert!(t.supported());
    }
}

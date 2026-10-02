//! File system building blocks for changes (PLAN 4.2, 4.3): clone (reflink or copy), rename
//! without replacing, fsync, and moving whole trees between subvolumes.
//!
//! Every root share (`homes`, team folders) and `xlrx-state` is its own Btrfs subvolume, so
//! `rename` between them fails with EXDEV. Moving into the trash or back then means: clone the
//! tree (reflinks cost neither time nor space), check that the source did not change meanwhile,
//! and only then remove the source.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use super::fs::SERVER_TEMP_PREFIX;

/// Layout below the state directory (`xlrx-state`).
pub const STAGING: &str = "store/staging";
pub const VERSIONS: &str = "store/versions";
pub const TRASH: &str = "store/trash";

/// Creates the store directories.
pub fn init(state_dir: &Path) -> io::Result<()> {
    for d in [STAGING, VERSIONS, TRASH] {
        fs::create_dir_all(state_dir.join(d))?;
    }
    Ok(())
}

/// A fresh, unique name for a short-lived file of the server (never synced or scanned).
pub fn temp_name() -> String {
    format!("{SERVER_TEMP_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// Relative path of a version in the content-addressed store: `store/versions/ab/cd/<hash>`.
pub fn version_rel(hash: &[u8; 32]) -> String {
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    format!("{VERSIONS}/{}/{}/{hex}", &hex[..2], &hex[2..4])
}

/// Copies `src` to the new file `dst` (which must not exist): reflink if the file system can,
/// otherwise a normal copy. The copy is flushed to disk. Returns whether a reflink was made.
pub fn clone_file(src: &Path, dst: &Path) -> io::Result<bool> {
    let from = File::open(src)?;
    let to = OpenOptions::new().write(true).create_new(true).open(dst)?;
    let result = (|| {
        let reflinked = match rustix::fs::ioctl_ficlone(&to, &from) {
            Ok(()) => true,
            Err(_) => {
                io::copy(&mut &from, &mut &to)?;
                false
            }
        };
        to.sync_all()?;
        Ok(reflinked)
    })();
    if result.is_err() {
        let _ = fs::remove_file(dst);
    }
    result
}

/// `rename` that never replaces an existing entry (RENAME_NOREPLACE). Fails with
/// `AlreadyExists` if `dst` exists.
pub fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE) {
        Ok(()) => Ok(()),
        // File system without RENAME_NOREPLACE (e.g. eCryptfs): files via link + unlink (link
        // fails if the target exists), directories with a check right before.
        Err(e) if e == rustix::io::Errno::INVAL || e == rustix::io::Errno::NOSYS => {
            if fs::symlink_metadata(src)?.is_dir() {
                if fs::symlink_metadata(dst).is_ok() {
                    return Err(io::ErrorKind::AlreadyExists.into());
                }
                fs::rename(src, dst)
            } else {
                fs::hard_link(src, dst)?;
                fs::remove_file(src)
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// Swaps two entries atomically (RENAME_EXCHANGE). `Unsupported` if the file system cannot.
pub fn exchange(a: &Path, b: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    match renameat_with(CWD, a, CWD, b, RenameFlags::EXCHANGE) {
        Ok(()) => Ok(()),
        Err(e) if e == rustix::io::Errno::INVAL || e == rustix::io::Errno::NOSYS => {
            Err(io::ErrorKind::Unsupported.into())
        }
        Err(e) => Err(e.into()),
    }
}

/// Flushes a directory entry (after create, rename, unlink).
pub fn fsync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// Is there an entry in `dir` whose name differs from `name` only in case or normalization?
/// (Btrfs is case-sensitive, Macs and SMB are not: such pairs would collide there.)
pub fn name_taken(dir: &Path, name: &str) -> io::Result<bool> {
    let key = super::db::fold(name);
    for e in fs::read_dir(dir)? {
        let e = e?;
        if let Some(n) = e.file_name().to_str()
            && super::db::fold(n) == key
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// How [`move_or_copy`] moved a tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    /// One atomic rename: done.
    Renamed,
    /// Copied and verified; the source still exists and must be removed with [`finish_copy`]
    /// (after the caller recorded that the copy is complete).
    Copied,
}

/// Moves a file or directory tree from `src` to `dst` (which must not exist). Same file
/// system: one atomic rename. Otherwise (EXDEV, or `force_copy` in tests): clone the tree, check
/// that nothing in the source changed meanwhile, and leave the source for [`finish_copy`]. If
/// anything changed, the copy is removed again and the source stays untouched.
///
/// With `stage`, the copy is built under that name (next to `dst`, a server temp name) and
/// renamed to `dst` only when complete, so `dst` never holds a partial tree.
pub fn move_or_copy(
    src: &Path,
    dst: &Path,
    stage: Option<&Path>,
    force_copy: bool,
) -> io::Result<Moved> {
    move_or_copy_with(src, dst, stage, force_copy, || {})
}

/// [`move_or_copy`] with a hook that runs right after copying (tests: a change meanwhile).
fn move_or_copy_with(
    src: &Path,
    dst: &Path,
    stage: Option<&Path>,
    force_copy: bool,
    after_copy: impl FnOnce(),
) -> io::Result<Moved> {
    if !force_copy {
        match rename_noreplace(src, dst) {
            Ok(()) => {
                sync_parents(src, dst)?;
                return Ok(Moved::Renamed);
            }
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {}
            Err(e) => return Err(e),
        }
    }
    if fs::symlink_metadata(dst).is_ok() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let build = stage.unwrap_or(dst);
    // Only what this call created may be removed again (the target could appear meanwhile).
    let mut created = false;
    let copied = (|| {
        let before = snapshot(src)?;
        copy_tree(src, build, &mut created)?;
        after_copy();
        if snapshot(src)? != before {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "während des Verschiebens geändert",
            ));
        }
        if let Some(stage) = stage {
            rename_noreplace(stage, dst)?;
        }
        sync_parents(dst, dst)
    })();
    if let Err(e) = copied {
        if created {
            let _ = remove_tree(build);
        }
        return Err(e);
    }
    Ok(Moved::Copied)
}

/// Removes the source after a verified copy ([`Moved::Copied`]).
pub fn finish_copy(src: &Path) -> io::Result<()> {
    remove_tree(src)?;
    sync_parents(src, src)
}

fn sync_parents(a: &Path, b: &Path) -> io::Result<()> {
    for p in [a.parent(), b.parent()].into_iter().flatten() {
        fsync_dir(p)?;
    }
    Ok(())
}

/// Removes a file or a directory tree.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Identity and fingerprint of every entry below `root`, to notice changes during a copy.
type Snapshot = BTreeMap<PathBuf, (u64, u64, i64, i64, u32)>;

fn snapshot(root: &Path) -> io::Result<Snapshot> {
    let mut out = Snapshot::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        // `join("")` would append a slash, which fails for a file.
        let path = if rel.as_os_str().is_empty() {
            root.to_path_buf()
        } else {
            root.join(&rel)
        };
        let m = fs::symlink_metadata(&path)?;
        // Directories: their mtime changes when entries are added or removed.
        out.insert(
            rel.clone(),
            (
                m.ino(),
                m.size(),
                m.mtime() * 1_000_000_000 + m.mtime_nsec(),
                m.ctime() * 1_000_000_000 + m.ctime_nsec(),
                m.mode(),
            ),
        );
        if m.is_dir() {
            for e in fs::read_dir(&path)? {
                stack.push(rel.join(e?.file_name()));
            }
        }
    }
    Ok(out)
}

/// Copies `src` to the new entry `dst`; `created` turns true as soon as `dst` exists.
fn copy_tree(src: &Path, dst: &Path, created: &mut bool) -> io::Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_file() {
        clone_file(src, dst)?;
        *created = true;
        let f = File::options().write(true).open(dst)?;
        f.set_permissions(m.permissions())?;
        f.set_modified(m.modified()?)?;
    } else if m.is_dir() {
        fs::create_dir(dst)?;
        *created = true;
        for e in fs::read_dir(src)? {
            let e = e?;
            copy_tree(&e.path(), &dst.join(e.file_name()), &mut true)?;
        }
        fs::set_permissions(dst, m.permissions())?;
        fsync_dir(dst)?;
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
        *created = true;
    } else {
        // Sockets, FIFOs, devices: never silently dropped, so the move fails instead.
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{}: Sonderdatei", src.display()),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_never_replaces() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        let e = rename_noreplace(&a, &b).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&b).unwrap(), "b");
        rename_noreplace(&a, &d.path().join("c")).unwrap();
        assert!(!a.exists());
    }

    #[test]
    fn exchange_swaps() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, "a").unwrap();
        fs::write(&b, "b").unwrap();
        exchange(&a, &b).unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "b");
        assert_eq!(fs::read_to_string(&b).unwrap(), "a");
    }

    #[test]
    fn clone_never_replaces() {
        let d = tempfile::tempdir().unwrap();
        let (a, b) = (d.path().join("a"), d.path().join("b"));
        fs::write(&a, "neu").unwrap();
        fs::write(&b, "alt").unwrap();
        assert!(clone_file(&a, &b).is_err());
        assert_eq!(fs::read_to_string(&b).unwrap(), "alt");
        clone_file(&a, &d.path().join("c")).unwrap();
        assert_eq!(fs::read_to_string(d.path().join("c")).unwrap(), "neu");
    }

    #[test]
    fn copy_keeps_tree_and_times() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Ordner");
        fs::create_dir_all(src.join("Unter")).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        fs::write(src.join("Unter/b.txt"), "b").unwrap();
        std::os::unix::fs::symlink("a.txt", src.join("verweis")).unwrap();
        let mtime = fs::metadata(src.join("a.txt")).unwrap().modified().unwrap();
        let dst = d.path().join("Ziel");
        let stage = d.path().join(".xlrx-srv-1");
        assert_eq!(
            move_or_copy(&src, &dst, Some(&stage), true).unwrap(),
            Moved::Copied
        );
        assert!(
            src.join("Unter/b.txt").exists(),
            "source stays until finish_copy"
        );
        assert!(!stage.exists());
        finish_copy(&src).unwrap();
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(dst.join("Unter/b.txt")).unwrap(), "b");
        assert_eq!(
            fs::read_link(dst.join("verweis")).unwrap(),
            Path::new("a.txt")
        );
        assert_eq!(
            fs::metadata(dst.join("a.txt")).unwrap().modified().unwrap(),
            mtime
        );
    }

    #[test]
    fn rename_when_possible() {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("a.txt"), d.path().join("b.txt"));
        fs::write(&src, "a").unwrap();
        assert_eq!(
            move_or_copy(&src, &dst, None, false).unwrap(),
            Moved::Renamed
        );
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(&dst).unwrap(), "a");
    }

    #[test]
    fn existing_target_is_never_touched() {
        let d = tempfile::tempdir().unwrap();
        let (src, dst) = (d.path().join("a"), d.path().join("b"));
        fs::write(&src, "a").unwrap();
        fs::write(&dst, "b").unwrap();
        for force in [false, true] {
            assert!(move_or_copy(&src, &dst, None, force).is_err());
            assert_eq!(fs::read_to_string(&dst).unwrap(), "b");
            assert_eq!(fs::read_to_string(&src).unwrap(), "a");
        }
        // The build location appears after the check (race): it is not ours, so it stays.
        let stage = d.path().join(".xlrx-srv-2");
        fs::remove_file(&dst).unwrap();
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("fremd.txt"), "fremd").unwrap();
        assert!(move_or_copy(&src, &dst, Some(&stage), true).is_err());
        assert_eq!(fs::read_to_string(&src).unwrap(), "a");
        assert_eq!(
            fs::read_to_string(stage.join("fremd.txt")).unwrap(),
            "fremd"
        );
    }

    #[test]
    fn change_during_copy_keeps_source() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Ordner");
        fs::create_dir_all(src.join("Unter")).unwrap();
        fs::write(src.join("Unter/a.txt"), "a").unwrap();
        let dst = d.path().join("Ziel");
        type Change = fn(&Path);
        let changes: [Change; 3] = [
            |s| fs::write(s.join("Unter/a.txt"), "A").unwrap(),
            |s| fs::write(s.join("Unter/neu.txt"), "neu").unwrap(),
            |s| fs::remove_file(s.join("Unter/a.txt")).unwrap(),
        ];
        for change in changes {
            let e = move_or_copy_with(&src, &dst, None, true, || change(&src)).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::Interrupted);
            assert!(!dst.exists(), "copy removed again");
            assert!(src.join("Unter").exists(), "source untouched");
        }
    }

    #[test]
    fn special_files_stop_the_copy() {
        let d = tempfile::tempdir().unwrap();
        let src = d.path().join("Ordner");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        let sock = std::os::unix::net::UnixListener::bind(src.join("sock")).unwrap();
        let e = move_or_copy(&src, &d.path().join("Ziel"), None, true).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::Unsupported);
        assert!(src.join("a.txt").exists());
        assert!(!d.path().join("Ziel").exists());
        drop(sock);
    }

    #[test]
    fn version_paths_fan_out() {
        let mut h = [0u8; 32];
        h[0] = 0xab;
        h[1] = 0xcd;
        let p = version_rel(&h);
        assert!(p.starts_with("store/versions/ab/cd/abcd00"));
        assert_eq!(p.len(), "store/versions/ab/cd/".len() + 64);
    }

    #[test]
    fn case_variants_count_as_taken() {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("Bericht.PDF"), "x").unwrap();
        assert!(name_taken(d.path(), "bericht.pdf").unwrap());
        assert!(!name_taken(d.path(), "bericht2.pdf").unwrap());
    }
}

//! The file system calls the client's scanner and executor need (ADR 0002 §7.1), as a thin, safe
//! wrapper around `rustix`: everything relative to an open directory, never following a symbolic
//! link, renames that refuse to overwrite or swap atomically, and syncing that reaches the disk
//! (`F_FULLFSYNC` on Apple). Linux and Apple only.
//!
//! Errors are plain `std::io::Error`s; the callers decide what an `EEXIST`, `ENOTEMPTY` or
//! `ELOOP` means for them.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags, Timespec, Timestamps};

/// Kind of a directory entry, without following links.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileKind {
    File,
    Dir,
    Symlink,
    /// FIFO, socket, device.
    Other,
}

impl FileKind {
    fn of(t: FileType) -> Self {
        match t {
            FileType::RegularFile => Self::File,
            FileType::Directory => Self::Dir,
            FileType::Symlink => Self::Symlink,
            _ => Self::Other,
        }
    }
}

/// What `stat` says about an object (never about the target of a link).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub dev: u64,
    pub ino: u64,
    pub kind: FileKind,
    pub nlink: u64,
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
    /// Time of creation, where the file system records it.
    pub btime_ns: Option<i64>,
    /// `st_flags` on Apple (e.g. `SF_DATALESS` for iCloud placeholders), 0 elsewhere.
    pub flags: u32,
}

/// How [`Dir::rename`] treats an existing target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenameMode {
    /// Fail with `EEXIST` if the target exists (`RENAME_NOREPLACE` / `RENAME_EXCL`).
    NoReplace,
    /// Swap source and target atomically; both must exist (`RENAME_EXCHANGE` / `RENAME_SWAP`).
    Exchange,
    /// Plain `renameat`: replaces an existing target. Only for changing the spelling of the same
    /// object on a case-insensitive volume, after proving it is the same object.
    Plain,
}

/// The file system a directory lies on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsType {
    Apfs,
    Ext4,
    Btrfs,
    Xfs,
    Tmpfs,
    /// Anything else, by name (Apple) or magic number (Linux).
    Other(String),
}

impl FsType {
    /// The file systems a sync folder may lie on (ADR 0002 §7.2): atomic swap, no-replace rename,
    /// stable inode numbers. APFS on the Mac; ext4, Btrfs, XFS and tmpfs on Linux (development).
    pub fn supported(&self) -> bool {
        !matches!(self, Self::Other(_))
    }
}

/// An open directory: the anchor of every other call.
#[derive(Debug)]
pub struct Dir {
    fd: OwnedFd,
}

const DIR_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

impl Dir {
    /// Opens a directory by path. The last component must not be a symbolic link.
    pub fn open(path: &Path) -> io::Result<Self> {
        let fd = rustix::fs::open(path, DIR_FLAGS, Mode::empty())?;
        Ok(Self { fd })
    }

    /// Opens a subdirectory (not through a symbolic link).
    pub fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        let fd = rustix::fs::openat(&self.fd, name, DIR_FLAGS, Mode::empty())?;
        Ok(Self { fd })
    }

    /// The directory itself.
    pub fn meta(&self) -> io::Result<Meta> {
        meta_fd(&self.fd)
    }

    /// An entry of the directory, without following a link.
    pub fn stat(&self, name: &OsStr) -> io::Result<Meta> {
        stat_at(&self.fd, name)
    }

    /// The names in the directory (without "." and ".."), with the kind if the file system tells
    /// it cheaply. The kind is a hint: [`Dir::stat`] is the truth.
    pub fn entries(&self) -> io::Result<Vec<(OsString, Option<FileKind>)>> {
        let mut dir = rustix::fs::Dir::read_from(&self.fd)?;
        let mut out = Vec::new();
        while let Some(entry) = dir.read() {
            let entry = entry?;
            let name = entry.file_name().to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            let kind = match entry.file_type() {
                FileType::Unknown => None,
                t => Some(FileKind::of(t)),
            };
            out.push((OsStr::from_bytes(name).to_owned(), kind));
        }
        Ok(out)
    }

    /// Creates a new file for writing; fails with `EEXIST` if the name exists (also as a link).
    pub fn create_new(&self, name: &OsStr) -> io::Result<File> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o644),
        )?;
        Ok(File::from(fd))
    }

    /// Opens a file for reading, not through a link. Non-blocking, so that a FIFO that took the
    /// file's place does not hang the caller; check the kind with [`meta_of`] afterwards.
    pub fn open_file(&self, name: &OsStr) -> io::Result<File> {
        let fd = rustix::fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(File::from(fd))
    }

    /// Renames `from` in this directory to `to` in `to_dir`.
    pub fn rename(
        &self,
        from: &OsStr,
        to_dir: &Dir,
        to: &OsStr,
        mode: RenameMode,
    ) -> io::Result<()> {
        match mode {
            RenameMode::Plain => rustix::fs::renameat(&self.fd, from, &to_dir.fd, to)?,
            RenameMode::NoReplace => {
                rustix::fs::renameat_with(&self.fd, from, &to_dir.fd, to, RenameFlags::NOREPLACE)?
            }
            RenameMode::Exchange => {
                rustix::fs::renameat_with(&self.fd, from, &to_dir.fd, to, RenameFlags::EXCHANGE)?
            }
        }
        Ok(())
    }

    pub fn mkdir(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::mkdirat(
            &self.fd,
            name,
            Mode::from_raw_mode(0o755),
        )?)
    }

    /// Removes an empty directory (`ENOTEMPTY` otherwise: the kernel checks atomically).
    pub fn rmdir(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)?)
    }

    /// Removes a file. Only for files proven to be the client's own.
    pub fn unlink(&self, name: &OsStr) -> io::Result<()> {
        Ok(rustix::fs::unlinkat(&self.fd, name, AtFlags::empty())?)
    }

    /// Makes the directory's entries (creations, renames, removals) durable.
    pub fn sync(&self) -> io::Result<()> {
        Ok(rustix::fs::fsync(&self.fd)?)
    }

    /// Sets the modification time of an entry (not through a link).
    pub fn set_mtime(&self, name: &OsStr, mtime_ns: i64) -> io::Result<()> {
        let at = Timespec {
            tv_sec: mtime_ns.div_euclid(1_000_000_000),
            tv_nsec: mtime_ns.rem_euclid(1_000_000_000) as _,
        };
        let times = Timestamps {
            last_access: at,
            last_modification: at,
        };
        Ok(rustix::fs::utimensat(
            &self.fd,
            name,
            &times,
            AtFlags::SYMLINK_NOFOLLOW,
        )?)
    }

    /// The file system the directory lies on.
    pub fn fs_type(&self) -> io::Result<FsType> {
        fs_type(&self.fd)
    }
}

/// `stat` of an open file.
pub fn meta_of(file: &File) -> io::Result<Meta> {
    meta_fd(file)
}

/// Makes a file's content and size durable. On Apple `F_FULLFSYNC`: plain `fsync` there only
/// reaches the drive's cache.
pub fn sync_file(file: &File) -> io::Result<()> {
    #[cfg(target_vendor = "apple")]
    rustix::fs::fcntl_fullfsync(file)?;
    #[cfg(not(target_vendor = "apple"))]
    rustix::fs::fsync(file)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn from_statx(s: &rustix::fs::Statx) -> Meta {
    use rustix::fs::StatxFlags;
    let ns = |t: rustix::fs::StatxTimestamp| t.tv_sec * 1_000_000_000 + i64::from(t.tv_nsec);
    Meta {
        dev: (u64::from(s.stx_dev_major) << 32) | u64::from(s.stx_dev_minor),
        ino: s.stx_ino,
        kind: FileKind::of(FileType::from_raw_mode(u32::from(s.stx_mode))),
        nlink: u64::from(s.stx_nlink),
        size: s.stx_size,
        mtime_ns: ns(s.stx_mtime),
        ctime_ns: ns(s.stx_ctime),
        btime_ns: StatxFlags::from_bits_retain(s.stx_mask)
            .contains(StatxFlags::BTIME)
            .then(|| ns(s.stx_btime)),
        flags: 0,
    }
}

#[cfg(target_os = "linux")]
fn stat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<Meta> {
    use rustix::fs::StatxFlags;
    let s = rustix::fs::statx(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    Ok(from_statx(&s))
}

#[cfg(target_os = "linux")]
fn meta_fd(fd: impl AsFd) -> io::Result<Meta> {
    use rustix::fs::StatxFlags;
    let s = rustix::fs::statx(
        fd,
        c"",
        AtFlags::EMPTY_PATH,
        StatxFlags::BASIC_STATS | StatxFlags::BTIME,
    )?;
    Ok(from_statx(&s))
}

#[cfg(target_os = "linux")]
fn fs_type(fd: &OwnedFd) -> io::Result<FsType> {
    let s = rustix::fs::fstatfs(fd)?;
    #[allow(clippy::unnecessary_cast)]
    let magic = s.f_type as u64;
    Ok(match magic {
        0xEF53 => FsType::Ext4,
        0x9123_683E => FsType::Btrfs,
        0x5846_5342 => FsType::Xfs,
        0x0102_1994 => FsType::Tmpfs,
        other => FsType::Other(format!("{other:#x}")),
    })
}

#[cfg(target_vendor = "apple")]
fn from_stat(s: &rustix::fs::Stat) -> Meta {
    Meta {
        dev: u64::from(s.st_dev as u32),
        ino: s.st_ino,
        kind: FileKind::of(FileType::from_raw_mode(s.st_mode)),
        nlink: u64::from(s.st_nlink),
        size: s.st_size as u64,
        mtime_ns: s.st_mtime * 1_000_000_000 + s.st_mtime_nsec,
        ctime_ns: s.st_ctime * 1_000_000_000 + s.st_ctime_nsec,
        btime_ns: Some(s.st_birthtime * 1_000_000_000 + s.st_birthtime_nsec),
        flags: s.st_flags,
    }
}

#[cfg(target_vendor = "apple")]
fn stat_at(dir: &OwnedFd, name: &OsStr) -> io::Result<Meta> {
    Ok(from_stat(&rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?))
}

#[cfg(target_vendor = "apple")]
fn meta_fd(fd: impl AsFd) -> io::Result<Meta> {
    Ok(from_stat(&rustix::fs::fstat(fd)?))
}

#[cfg(target_vendor = "apple")]
fn fs_type(fd: &OwnedFd) -> io::Result<FsType> {
    let s = rustix::fs::fstatfs(fd)?;
    let name: Vec<u8> = s
        .f_fstypename
        .iter()
        .take_while(|c| **c != 0)
        .map(|c| *c as u8)
        .collect();
    Ok(match name.as_slice() {
        b"apfs" => FsType::Apfs,
        other => FsType::Other(String::from_utf8_lossy(other).into_owned()),
    })
}

#[cfg(test)]
mod tests;

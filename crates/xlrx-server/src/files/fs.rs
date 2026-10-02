//! Reading the file tree on disk: what to ignore, and a stat-only walk of a directory tree.

use std::path::{Path, PathBuf};

use time::OffsetDateTime;
use xlrx_chunk::{FileId, Fingerprint, fingerprint_of};
use xlrx_proto::Name;

/// Prefix of the server's own short-lived files next to user files (atomic writes, PLAN 4.3).
/// Distinct from the clients' `.xlrx-tmp-` yield names, which are regular synced names.
pub const SERVER_TEMP_PREFIX: &str = ".xlrx-srv-";

/// Names that never become nodes: Synology helper directories, OS metadata, lock files and our
/// own temporary files (PLAN 4.4).
pub fn ignored(name: &str) -> bool {
    matches!(
        name,
        "@eaDir"
            | "#recycle"
            | "#snapshot"
            | "@tmp"
            | ".SynologyWorkingDirectory"
            | ".DS_Store"
            | "Thumbs.db"
            | "desktop.ini"
            | ".Spotlight-V100"
            | ".Trashes"
            | ".fseventsd"
    ) || name.starts_with("._")
        || name.starts_with("~$")
        || name.starts_with(".~lock.")
        || name.starts_with(SERVER_TEMP_PREFIX)
        || name.starts_with(xlrx_sync::DOWNLOAD_TEMP_PREFIX)
}

/// An entry found on disk.
#[derive(Clone, Debug)]
pub struct Seen {
    /// Path relative to the root directory (empty for the root itself).
    pub rel: PathBuf,
    /// Exact name on disk (valid UTF-8, checked with [`Name::new`]).
    pub name: String,
    pub is_dir: bool,
    pub id: FileId,
    pub fp: Fingerprint,
    pub mtime: Option<OffsetDateTime>,
}

impl Seen {
    pub fn parent_rel(&self) -> Option<&Path> {
        (!self.rel.as_os_str().is_empty()).then(|| self.rel.parent().unwrap_or(Path::new("")))
    }
}

/// Result of a walk: entries in breadth-first order (parents before children) and names that had
/// to be skipped.
#[derive(Default)]
pub struct Walk {
    pub entries: Vec<Seen>,
    pub skipped: Vec<String>,
}

fn stat(path: &Path, rel: PathBuf, name: String) -> std::io::Result<Option<Seen>> {
    let meta = std::fs::symlink_metadata(path)?;
    // Symlinks and special files are never followed or synced.
    if !(meta.is_dir() || meta.is_file()) {
        return Ok(None);
    }
    let (id, fp) = fingerprint_of(&meta);
    Ok(Some(Seen {
        rel,
        name,
        is_dir: meta.is_dir(),
        id,
        fp,
        mtime: meta.modified().ok().map(OffsetDateTime::from),
    }))
}

/// Walks `root` (which must be a directory). With `recursive = false` only the direct children
/// of `start` are listed.
pub fn walk(root: &Path, start: &Path, recursive: bool) -> std::io::Result<Walk> {
    let mut out = Walk::default();
    let top = root.join(start);
    let name = start
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stat(&top, start.to_path_buf(), name)? {
        Some(s) if s.is_dir => out.entries.push(s),
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotADirectory,
                format!("{} ist kein Ordner", top.display()),
            ));
        }
    }
    let mut queue = std::collections::VecDeque::from([start.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let rd = match std::fs::read_dir(root.join(&dir)) {
            Ok(rd) => rd,
            Err(e) => {
                out.skipped.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        let mut children: Vec<_> = rd.flatten().collect();
        children.sort_by_key(|e| e.file_name());
        for e in children {
            let raw = e.file_name();
            let Some(name) = raw.to_str() else {
                out.skipped
                    .push(format!("{}: Name ist kein UTF-8", dir.join(&raw).display()));
                continue;
            };
            if ignored(name) {
                continue;
            }
            if Name::new(name).is_err() {
                out.skipped
                    .push(format!("{}: unzulässiger Name", dir.join(name).display()));
                continue;
            }
            let rel = dir.join(name);
            match stat(&root.join(&rel), rel.clone(), name.to_owned()) {
                Ok(Some(s)) => {
                    if s.is_dir && recursive {
                        queue.push_back(rel);
                    }
                    out.entries.push(s);
                }
                Ok(None) => {}
                // Vanished between listing and stat: the next scan sees the new state.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => out.skipped.push(format!("{}: {e}", rel.display())),
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_list() {
        for n in [
            "@eaDir",
            ".DS_Store",
            "._Bericht.pdf",
            "~$Bericht.docx",
            ".xlrx-srv-1",
            ".xlrx-dl-2",
        ] {
            assert!(ignored(n), "{n}");
        }
        for n in ["Bericht.pdf", ".xlrx-tmp-Mac-1~a", ".bashrc", "#1 Liste"] {
            assert!(!ignored(n), "{n}");
        }
    }
}

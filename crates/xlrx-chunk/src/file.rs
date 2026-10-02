//! Dateien hashen, ohne veränderte Dateien fälschlich als stabil zu melden.

use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

use crate::{Chunker, Digest};

/// Identität einer Datei auf dem Gerät (Gerät + Inode). Bleibt bei Umbenennen und Verschieben gleich.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
}

/// Merkmale, die sich bei jeder Inhaltsänderung ändern.
///
/// `ctime` lässt sich von Programmen nicht zurücksetzen (anders als `mtime`). Damit fallen auch
/// Werkzeuge auf, die nach dem Schreiben die alte Änderungszeit wiederherstellen.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Fingerprint {
    pub size: u64,
    pub mtime_ns: i64,
    pub ctime_ns: i64,
}

/// Liest Identität und Fingerprint aus Metadaten.
#[cfg(unix)]
pub fn fingerprint_of(meta: &Metadata) -> (FileId, Fingerprint) {
    use std::os::unix::fs::MetadataExt;
    let ns = |s: i64, n: i64| s.saturating_mul(1_000_000_000).saturating_add(n);
    (
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
        },
        Fingerprint {
            size: meta.size(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        },
    )
}

/// Ergebnis von [`digest_file`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FileDigest {
    /// Die Datei war während des gesamten Lesens unverändert. Der Hash gehört zu genau diesem Fingerprint.
    Stable {
        id: FileId,
        fingerprint: Fingerprint,
        digest: Digest,
    },
    /// Die Datei wurde während des Lesens verändert. Später erneut versuchen.
    ChangedDuringRead,
}

/// Hasht eine Datei und prüft vorher und nachher, ob sie unverändert geblieben ist.
///
/// Ein Hash wird nur dann einem Fingerprint zugeordnet, wenn beide Messungen übereinstimmen und die
/// gelesene Länge zur Dateigröße passt. Halb geschriebene Dateien werden so nie als stabil gemeldet.
#[cfg(unix)]
pub fn digest_file(chunker: &mut Chunker, path: &Path) -> io::Result<FileDigest> {
    let file = File::open(path)?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "keine reguläre Datei",
        ));
    }
    let digest = chunker.digest_reader(&file)?;
    let after = file.metadata()?;
    let (id, fp_before) = fingerprint_of(&before);
    let (_, fp_after) = fingerprint_of(&after);
    if fp_before != fp_after || digest.content.size != fp_after.size {
        return Ok(FileDigest::ChangedDuringRead);
    }
    Ok(FileDigest::Stable {
        id,
        fingerprint: fp_after,
        digest,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stable_file_is_reported_with_its_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, crate::tests::pseudo_random(5 * 1024 * 1024 + 7, 1)).unwrap();
        let mut c = Chunker::new();
        match digest_file(&mut c, &p).unwrap() {
            FileDigest::Stable {
                fingerprint,
                digest,
                ..
            } => {
                assert_eq!(fingerprint.size, 5 * 1024 * 1024 + 7);
                assert_eq!(digest.content.size, fingerprint.size);
            }
            FileDigest::ChangedDuringRead => panic!("Datei war unverändert"),
        }
    }

    #[test]
    fn fingerprint_changes_on_rewrite_with_same_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.txt");
        std::fs::write(&p, b"aaaa").unwrap();
        let (_, a) = fingerprint_of(&std::fs::metadata(&p).unwrap());
        std::thread::sleep(std::time::Duration::from_millis(5));
        let mut f = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        f.write_all(b"bbbb").unwrap();
        drop(f);
        let (_, b) = fingerprint_of(&std::fs::metadata(&p).unwrap());
        assert_eq!(a.size, b.size);
        assert_ne!(a, b, "ctime/mtime müssen sich ändern");
    }

    #[test]
    fn directories_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = Chunker::new();
        assert!(digest_file(&mut c, dir.path()).is_err());
    }
}

//! Identity of a local object (ADR 0002 §7.3): stays the same across renames and moves, and a
//! new object never inherits an old one's identity.

use xlrx_fs::Meta;
use xlrx_sync::LocalId;

/// The engine's identity for an object.
///
/// - Apple: the APFS file ID (`st_ino`), which APFS does not reuse.
/// - Linux: ext4 reuses inode numbers quickly; mixing in the birth time gives a recreated file a
///   new identity. Without a birth time it falls back to the inode number: a reuse by an object
///   of the same kind then looks like "moved and changed", which is safe (upload, no loss).
///
/// Never [`LocalId::GONE`]. The executor always recomputes an identity from a fresh `stat`.
pub fn local_id(meta: &Meta) -> LocalId {
    #[cfg(target_vendor = "apple")]
    let id = meta.ino;
    #[cfg(not(target_vendor = "apple"))]
    let id = match meta.btime_ns {
        Some(b) => splitmix64(meta.ino ^ (b as u64).rotate_left(32)),
        None => meta.ino,
    };
    if id == LocalId::GONE.0 {
        LocalId(id - 1)
    } else {
        LocalId(id)
    }
}

/// A fixed 64-bit mixing function (SplitMix64's finalizer): neighbouring inputs give unrelated
/// outputs, so that `ino ^ btime` collisions are as unlikely as random ones.
#[cfg(not(target_vendor = "apple"))]
fn splitmix64(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::io::Write;

    use xlrx_fs::Dir;

    use super::*;

    #[test]
    fn bleibt_beim_umbenennen_neu_bei_neuer_datei() {
        let t = tempfile::tempdir().unwrap();
        let d = Dir::open(t.path()).unwrap();
        d.create_new(OsStr::new("a"))
            .unwrap()
            .write_all(b"a")
            .unwrap();
        let first = local_id(&d.stat(OsStr::new("a")).unwrap());
        d.rename(
            OsStr::new("a"),
            &d,
            OsStr::new("b"),
            xlrx_fs::RenameMode::NoReplace,
        )
        .unwrap();
        assert_eq!(local_id(&d.stat(OsStr::new("b")).unwrap()), first);
        // Delete and create again: often the same inode number on ext4, never the same identity
        // where the birth time is known.
        d.unlink(OsStr::new("b")).unwrap();
        d.create_new(OsStr::new("b")).unwrap();
        let again = d.stat(OsStr::new("b")).unwrap();
        if again.btime_ns.is_some() {
            assert_ne!(local_id(&again), first);
        }
    }

    #[cfg(not(target_vendor = "apple"))]
    #[test]
    fn nie_gone() {
        let mut m = Dir::open(std::path::Path::new("/"))
            .unwrap()
            .meta()
            .unwrap();
        m.btime_ns = None;
        m.ino = u64::MAX;
        assert_ne!(local_id(&m), LocalId::GONE);
    }
}

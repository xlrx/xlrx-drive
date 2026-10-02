//! Misst Scan- und Hash-Geschwindigkeit auf echten Daten (z.B. für den DS918+-Spike).
//!
//! ```text
//! cargo run --release -p xlrx-chunk --example hashdir -- /volume1/homes/klaus/Drive
//! ```
//!
//! Durchlauf 1 hasht alles. Durchlauf 2 nutzt den Hash-Cache und zeigt den Schnellweg für
//! unveränderte Dateien (nur `stat`, kein Lesen).

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rayon::prelude::*;
use xlrx_chunk::{CacheEntry, Chunker, FileDigest, HashCache, digest_file, fingerprint_of};

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            walk(&e.path(), out);
        } else if ft.is_file() {
            out.push(e.path());
        }
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn main() {
    let root = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    let t = Instant::now();
    let mut files = Vec::new();
    walk(&root, &mut files);
    println!("{} Dateien gefunden in {:.2?}", files.len(), t.elapsed());

    let cache = Mutex::new(HashCache::new());
    for round in 1..=2 {
        let t = Instant::now();
        let bytes = std::sync::atomic::AtomicU64::new(0);
        let hashed = std::sync::atomic::AtomicU64::new(0);
        files.par_iter().for_each_init(Chunker::new, |chunker, p| {
            let Ok(meta) = std::fs::symlink_metadata(p) else {
                return;
            };
            let (id, fp) = fingerprint_of(&meta);
            if cache
                .lock()
                .map(|c| c.lookup(id, fp).is_some())
                .unwrap_or(false)
            {
                return;
            }
            let hashed_at_ns = now_ns();
            if let Ok(FileDigest::Stable {
                id,
                fingerprint,
                digest,
            }) = digest_file(chunker, p)
            {
                bytes.fetch_add(digest.content.size, std::sync::atomic::Ordering::Relaxed);
                hashed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut c) = cache.lock() {
                    c.insert(
                        id,
                        CacheEntry {
                            fingerprint,
                            hashed_at_ns,
                            content: digest.content,
                        },
                    );
                }
            }
        });
        let el = t.elapsed().as_secs_f64();
        let b = bytes.into_inner() as f64;
        println!(
            "Durchlauf {round}: {:.2} s, {} gehasht, {:.0} Dateien/s, {:.1} MB/s",
            el,
            hashed.into_inner(),
            files.len() as f64 / el,
            b / el / 1e6
        );
        // Kurz warten, damit frisch gehashte Dateien beim zweiten Durchlauf nicht als „racy“ gelten.
        if round == 1 {
            std::thread::sleep(std::time::Duration::from_millis(2100));
        }
    }
}

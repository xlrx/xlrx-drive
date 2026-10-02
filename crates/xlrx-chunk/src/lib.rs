//! Content-Defined Chunking und Hashing für xlrx-drive.
//!
//! # Schema `xlrx-content-v1`
//!
//! - Chunk-Grenzen: FastCDC 2020, Normalisierung Stufe 1, min 256 KiB / avg 1 MiB / max 4 MiB.
//! - Chunk-Hash: BLAKE3 der Chunk-Bytes.
//! - Inhalts-Hash: BLAKE3 im Derive-Key-Modus über (Größe, Anzahl Chunks, je Chunk: Hash und Länge).
//!
//! Der Inhalts-Hash wird aus der Chunk-Liste abgeleitet. So reicht **ein** Lesedurchgang pro Datei,
//! und große Dateien können parallel gehasht werden. Die Parameter sind Teil des Schemas und dürfen
//! sich nie ändern; ein Golden-Test sichert das ab.

mod cache;
mod file;

use std::io::{self, Read};

pub use cache::{CacheEntry, HashCache, RACY_WINDOW_NS};
pub use file::{FileDigest, FileId, Fingerprint};
#[cfg(unix)]
pub use file::{digest_file, fingerprint_of};
use xlrx_proto::{ContentHash, FileContent};

/// Kleinste Chunk-Größe (außer beim letzten Chunk einer Datei).
pub const CHUNK_MIN: usize = 256 * 1024;
/// Angestrebte durchschnittliche Chunk-Größe.
pub const CHUNK_AVG: usize = 1024 * 1024;
/// Größte Chunk-Größe.
pub const CHUNK_MAX: usize = 4 * 1024 * 1024;

/// Kontext für den Inhalts-Hash. Global eindeutig und fest, siehe BLAKE3 `derive_key`.
const CONTENT_CONTEXT: &str = "xlrx-drive 2026-10-02 content-v1";

/// Größe des Lesepuffers. Muss deutlich größer als [`CHUNK_MAX`] sein, damit pro Füllung mehrere
/// Chunks entstehen und der verbleibende Rest, der umkopiert wird, klein bleibt.
const BUF_SIZE: usize = 16 * 1024 * 1024;

/// Ab dieser Datenmenge pro Puffer lohnt sich paralleles Hashen der Chunks.
#[cfg(feature = "parallel")]
const PARALLEL_MIN_BYTES: usize = 2 * CHUNK_AVG;

/// BLAKE3-Hash eines einzelnen Chunks.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChunkHash(pub [u8; 32]);

impl std::fmt::Debug for ChunkHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "c")?;
        for b in &self.0[..6] {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Ein Chunk einer Datei.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkInfo {
    pub hash: ChunkHash,
    /// Position in der Datei.
    pub offset: u64,
    /// Länge in Bytes (höchstens [`CHUNK_MAX`]).
    pub len: u32,
}

/// Ergebnis des Hashens: Inhalt (Hash + Größe) und Chunk-Liste.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Digest {
    pub content: FileContent,
    pub chunks: Vec<ChunkInfo>,
}

/// Hasht einen einzelnen Chunk.
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(data).as_bytes())
}

/// Leitet den Inhalts-Hash aus Größe und Chunk-Liste ab (Schema `xlrx-content-v1`).
pub fn content_hash(size: u64, chunks: &[ChunkInfo]) -> ContentHash {
    let mut h = blake3::Hasher::new_derive_key(CONTENT_CONTEXT);
    h.update(&size.to_le_bytes());
    h.update(&(chunks.len() as u64).to_le_bytes());
    for c in chunks {
        h.update(&c.hash.0);
        h.update(&c.len.to_le_bytes());
    }
    ContentHash(*h.finalize().as_bytes())
}

/// Wiederverwendbarer Chunker. Hält den Lesepuffer, damit pro Datei nichts neu alloziert wird.
/// Für paralleles Hashen vieler Dateien: ein `Chunker` pro Thread.
pub struct Chunker {
    buf: Vec<u8>,
    mask_s: u64,
    mask_l: u64,
    ranges: Vec<(usize, usize)>,
    hashes: Vec<ChunkHash>,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        let (mask_s, mask_l) =
            fastcdc::v2020::select_masks(CHUNK_AVG, fastcdc::v2020::Normalization::Level1);
        Self {
            buf: Vec::new(),
            mask_s,
            mask_l,
            ranges: Vec::new(),
            hashes: Vec::new(),
        }
    }

    /// Länge des nächsten Chunks ab dem Anfang von `window`.
    ///
    /// `window` muss entweder mindestens [`CHUNK_MAX`] Bytes lang sein oder bis zum Dateiende reichen,
    /// sonst wäre der Schnittpunkt nicht endgültig.
    fn next_cut(&self, window: &[u8]) -> usize {
        let window = &window[..window.len().min(CHUNK_MAX)];
        let (_, len) = fastcdc::v2020::cut(
            window,
            CHUNK_MIN,
            CHUNK_AVG,
            CHUNK_MAX,
            self.mask_s,
            self.mask_l,
            self.mask_s << 1,
            self.mask_l << 1,
        );
        len
    }

    /// Hasht Daten, die vollständig im Speicher liegen (ohne Umkopieren).
    pub fn digest_slice(&mut self, data: &[u8]) -> Digest {
        self.ranges.clear();
        let mut pos = 0;
        while pos < data.len() {
            let len = self.next_cut(&data[pos..]);
            self.ranges.push((pos, len));
            pos += len;
        }
        let mut chunks = Vec::with_capacity(self.ranges.len());
        hash_ranges(data, &self.ranges, &mut self.hashes);
        for (&(p, l), &hash) in self.ranges.iter().zip(&self.hashes) {
            chunks.push(ChunkInfo {
                hash,
                offset: p as u64,
                len: l as u32,
            });
        }
        let size = data.len() as u64;
        Digest {
            content: FileContent {
                hash: content_hash(size, &chunks),
                size,
            },
            chunks,
        }
    }

    /// Liest bis zum Ende und hasht dabei in einem einzigen Durchgang.
    pub fn digest_reader<R: Read>(&mut self, mut reader: R) -> io::Result<Digest> {
        if self.buf.len() != BUF_SIZE {
            self.buf = vec![0; BUF_SIZE];
        }
        let mut start = 0usize; // Beginn des noch nicht verarbeiteten Bereichs im Puffer
        let mut end = 0usize; // Ende der gültigen Daten im Puffer
        let mut eof = false;
        let mut offset = 0u64; // Dateiposition von buf[start]
        let mut chunks = Vec::new();

        loop {
            if !eof && end - start < CHUNK_MAX {
                self.buf.copy_within(start..end, 0);
                end -= start;
                start = 0;
                while end < BUF_SIZE && !eof {
                    match reader.read(&mut self.buf[end..]) {
                        Ok(0) => eof = true,
                        Ok(n) => end += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            if start == end {
                break;
            }

            // Alle Schnittpunkte sammeln, die mit den vorhandenen Daten endgültig sind.
            self.ranges.clear();
            let mut pos = start;
            while pos < end && (eof || end - pos >= CHUNK_MAX) {
                let len = self.next_cut(&self.buf[pos..end]);
                self.ranges.push((pos, len));
                pos += len;
            }

            hash_ranges(&self.buf, &self.ranges, &mut self.hashes);
            for (&(p, l), &hash) in self.ranges.iter().zip(&self.hashes) {
                chunks.push(ChunkInfo {
                    hash,
                    offset: offset + (p - start) as u64,
                    len: l as u32,
                });
            }
            offset += (pos - start) as u64;
            start = pos;
        }

        Ok(Digest {
            content: FileContent {
                hash: content_hash(offset, &chunks),
                size: offset,
            },
            chunks,
        })
    }
}

/// Hasht die angegebenen Bereiche, bei genügend Daten parallel.
fn hash_ranges(buf: &[u8], ranges: &[(usize, usize)], out: &mut Vec<ChunkHash>) {
    out.clear();
    #[cfg(feature = "parallel")]
    {
        let total: usize = ranges.iter().map(|r| r.1).sum();
        if ranges.len() > 1 && total >= PARALLEL_MIN_BYTES {
            use rayon::prelude::*;
            ranges
                .par_iter()
                .map(|&(p, l)| chunk_hash(&buf[p..p + l]))
                .collect_into_vec(out);
            return;
        }
    }
    out.extend(ranges.iter().map(|&(p, l)| chunk_hash(&buf[p..p + l])));
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Deterministische Pseudozufallsdaten (xorshift64*), unabhängig von externen Crates.
    pub(crate) fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            out.extend_from_slice(&x.wrapping_mul(0x2545_F491_4F6C_DD1D).to_le_bytes());
        }
        out.truncate(len);
        out
    }

    /// Liefert Daten in unregelmäßigen, kleinen Häppchen, wie ein Netzwerk-Stream.
    struct Choppy<'a> {
        data: &'a [u8],
        pos: usize,
        step: usize,
    }

    impl Read for Choppy<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            self.step = (self.step * 7 + 13) % 70_001 + 1;
            let n = self.step.min(out.len()).min(self.data.len() - self.pos);
            out[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
            self.pos += n;
            Ok(n)
        }
    }

    fn check_structure(d: &Digest, size: usize) {
        assert_eq!(d.content.size, size as u64);
        let mut expected_offset = 0u64;
        for (i, c) in d.chunks.iter().enumerate() {
            assert_eq!(c.offset, expected_offset);
            assert!(c.len as usize <= CHUNK_MAX);
            if i + 1 < d.chunks.len() {
                assert!(
                    c.len as usize >= CHUNK_MIN,
                    "nur der letzte Chunk darf kleiner sein"
                );
            }
            expected_offset += u64::from(c.len);
        }
        assert_eq!(expected_offset, size as u64);
    }

    #[test]
    fn empty_input() {
        let mut c = Chunker::new();
        let d = c.digest_slice(&[]);
        assert!(d.chunks.is_empty());
        assert_eq!(d.content.size, 0);
        assert_eq!(c.digest_reader(&[][..]).unwrap(), d);
    }

    #[test]
    fn reader_and_slice_agree_on_large_input() {
        let data = pseudo_random(37 * 1024 * 1024 + 123, 42);
        let mut c = Chunker::new();
        let a = c.digest_slice(&data);
        let b = c.digest_reader(&data[..]).unwrap();
        let chopped = c
            .digest_reader(Choppy {
                data: &data,
                pos: 0,
                step: 1,
            })
            .unwrap();
        assert_eq!(a, b);
        assert_eq!(a, chopped);
        check_structure(&a, data.len());
        assert!(
            a.chunks.len() > 10,
            "Erwarte viele Chunks, bekam {}",
            a.chunks.len()
        );
    }

    #[test]
    fn insertion_only_changes_nearby_chunks() {
        let data = pseudo_random(24 * 1024 * 1024, 7);
        let mut shifted = b"ein paar neue Bytes am Anfang".to_vec();
        shifted.extend_from_slice(&data);
        let mut c = Chunker::new();
        let a = c.digest_slice(&data);
        let b = c.digest_slice(&shifted);
        let a_set: std::collections::HashSet<_> = a.chunks.iter().map(|c| c.hash).collect();
        let shared = b.chunks.iter().filter(|c| a_set.contains(&c.hash)).count();
        assert!(
            shared + 2 >= a.chunks.len(),
            "Einfügen am Anfang darf nur die ersten Chunks ändern: {shared} von {} gleich",
            a.chunks.len()
        );
    }

    #[test]
    fn content_hash_depends_on_everything() {
        let mut c = Chunker::new();
        let a = c.digest_slice(b"hallo");
        let b = c.digest_slice(b"hallO");
        assert_ne!(a.content.hash, b.content.hash);
        let empty = c.digest_slice(b"");
        assert_ne!(a.content.hash, empty.content.hash);
    }

    /// Schützt das Schema: Ändern sich Chunk-Grenzen oder Hash-Ableitung, schlägt dieser Test fehl.
    /// Dann darf NICHT einfach der Erwartungswert angepasst werden, sondern es braucht ein neues Schema.
    #[test]
    fn golden_schema_v1() {
        let data = pseudo_random(20 * 1024 * 1024, 2026);
        let d = Chunker::new().digest_slice(&data);
        let lens: Vec<u32> = d.chunks.iter().map(|c| c.len).collect();
        assert_eq!(lens, GOLDEN_LENS, "Chunk-Grenzen haben sich geändert");
        assert_eq!(
            d.content.hash.to_hex(),
            GOLDEN_HASH,
            "Inhalts-Hash hat sich geändert"
        );
    }

    const GOLDEN_LENS: &[u32] = &[
        1572305, 2170648, 658593, 1183046, 1941377, 1057263, 568288, 371617, 611090, 391628,
        1730074, 519238, 2642492, 1411586, 1253350, 1362868, 1036318, 489739,
    ];
    const GOLDEN_HASH: &str = "e4586192c62d039e01c7587eae36fc66ad5e2288557c2ad3a99ee5992c674638";

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]
        #[test]
        fn structure_holds(len in 0usize..(9 * 1024 * 1024), seed in any::<u64>()) {
            let data = pseudo_random(len, seed);
            let mut c = Chunker::new();
            let a = c.digest_slice(&data);
            check_structure(&a, len);
            let b = c.digest_reader(Choppy { data: &data, pos: 0, step: (seed % 1000) as usize }).unwrap();
            prop_assert_eq!(a, b);
        }
    }
}

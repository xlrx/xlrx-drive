//! Content-defined chunking and hashing for xlrx-drive.
//!
//! # Schema `xlrx-content-v1`
//!
//! - Chunk boundaries: FastCDC 2020, normalization level 1, min 256 KiB / avg 1 MiB / max 4 MiB.
//! - Chunk hash: BLAKE3 of the chunk bytes.
//! - Content hash: BLAKE3 in derive-key mode over (size, chunk count, per chunk: hash and length).
//!
//! The content hash is derived from the chunk list. That way, **one** read pass per file suffices,
//! and large files can be hashed in parallel. The parameters are part of the schema and must never
//! change; a golden test guards this.

mod cache;
mod file;

use std::io::{self, Read};

pub use cache::{CacheEntry, HashCache, RACY_WINDOW_NS};
pub use file::{FileDigest, FileId, Fingerprint};
#[cfg(unix)]
pub use file::{digest_file, fingerprint_of};
use xlrx_proto::{ContentHash, FileContent};

/// Smallest chunk size (except for the last chunk of a file).
pub const CHUNK_MIN: usize = 256 * 1024;
/// Target average chunk size.
pub const CHUNK_AVG: usize = 1024 * 1024;
/// Largest chunk size.
pub const CHUNK_MAX: usize = 4 * 1024 * 1024;

/// Context for the content hash. Globally unique and fixed, see BLAKE3 `derive_key`.
const CONTENT_CONTEXT: &str = "xlrx-drive 2026-10-02 content-v1";

/// Size of the read buffer. Must be much larger than [`CHUNK_MAX`] so that each fill yields
/// several chunks and the leftover that gets copied over stays small.
const BUF_SIZE: usize = 16 * 1024 * 1024;

/// From this amount of data per buffer on, hashing the chunks in parallel pays off.
#[cfg(feature = "parallel")]
const PARALLEL_MIN_BYTES: usize = 2 * CHUNK_AVG;

/// BLAKE3 hash of a single chunk.
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

/// A chunk of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ChunkInfo {
    pub hash: ChunkHash,
    /// Position in the file.
    pub offset: u64,
    /// Length in bytes (at most [`CHUNK_MAX`]).
    pub len: u32,
}

/// Result of hashing: content (hash + size) and chunk list.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Digest {
    pub content: FileContent,
    pub chunks: Vec<ChunkInfo>,
}

/// Hashes a single chunk.
pub fn chunk_hash(data: &[u8]) -> ChunkHash {
    ChunkHash(*blake3::hash(data).as_bytes())
}

/// Derives the content hash from size and chunk list (schema `xlrx-content-v1`).
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

/// Reusable chunker. Keeps the read buffer so that nothing is reallocated per file.
/// For hashing many files in parallel: one `Chunker` per thread.
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

    /// Length of the next chunk starting at the beginning of `window`.
    ///
    /// `window` must either be at least [`CHUNK_MAX`] bytes long or extend to the end of the file;
    /// otherwise the cut point would not be final.
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

    /// Hashes data that lies entirely in memory (without copying).
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

    /// Reads to the end, hashing in a single pass.
    pub fn digest_reader<R: Read>(&mut self, mut reader: R) -> io::Result<Digest> {
        if self.buf.len() != BUF_SIZE {
            self.buf = vec![0; BUF_SIZE];
        }
        let mut start = 0usize; // Start of the not yet processed region in the buffer
        let mut end = 0usize; // End of the valid data in the buffer
        let mut eof = false;
        let mut offset = 0u64; // File position of buf[start]
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

            // Collect all cut points that are final given the data available.
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

/// Hashes the given ranges, in parallel if there is enough data.
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

    /// Deterministic pseudo-random data (xorshift64*), independent of external crates.
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

    /// Delivers data in small, irregular pieces, like a network stream.
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

    /// Protects the schema: if the chunk boundaries or the hash derivation change, this test fails.
    /// In that case, do NOT simply adjust the expected value; a new schema is required instead.
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

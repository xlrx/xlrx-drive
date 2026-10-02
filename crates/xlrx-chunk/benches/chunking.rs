//! Durchsatz-Messungen für Chunking und Hashing.
//!
//! `cargo bench -p xlrx-chunk` – auf dem DS918+ zusätzlich `examples/hashdir.rs` mit echten Daten nutzen.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use xlrx_chunk::Chunker;

fn pseudo_random(len: usize, seed: u64) -> Vec<u8> {
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

fn large_file(c: &mut Criterion) {
    let data = pseudo_random(64 * 1024 * 1024, 1);
    let mut g = c.benchmark_group("large_file_64MiB");
    g.throughput(Throughput::Bytes(data.len() as u64));
    g.sample_size(10);
    let mut chunker = Chunker::new();
    g.bench_function("digest_slice", |b| {
        b.iter(|| black_box(chunker.digest_slice(black_box(&data))))
    });
    g.bench_function("digest_reader", |b| {
        b.iter(|| black_box(chunker.digest_reader(black_box(&data[..])).unwrap()))
    });
    g.bench_function("blake3_only", |b| {
        b.iter(|| black_box(blake3::hash(black_box(&data))))
    });
    g.finish();
}

fn small_files(c: &mut Criterion) {
    let files: Vec<Vec<u8>> = (0..1000).map(|i| pseudo_random(4096, i)).collect();
    let mut g = c.benchmark_group("small_files_1000x4KiB");
    g.throughput(Throughput::Elements(files.len() as u64));
    let mut chunker = Chunker::new();
    g.bench_function("digest_slice", |b| {
        b.iter(|| {
            for f in &files {
                black_box(chunker.digest_slice(black_box(f)));
            }
        })
    });
    g.finish();
}

criterion_group!(benches, large_file, small_files);
criterion_main!(benches);

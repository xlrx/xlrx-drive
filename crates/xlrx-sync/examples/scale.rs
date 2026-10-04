//! Measures the engine on large trees: initial setup (linking without transfer) and
//! a planning pass in the idle state.
//!
//! `cargo run --release -p xlrx-sync --example scale -- 1000000`

use std::time::Instant;

use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Rev, Seq};
use xlrx_sync::{
    Config, Engine, Fingerprint, LocalEntry, LocalId, LocalObservation, RemoteChange, RemoteEntry,
};

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(200_000);
    let per_dir = 100u64;
    let dirs = n / per_dir;
    let root = NodeId(1);
    let lroot = LocalId(1);
    let content = |i: u64| {
        let mut h = [0u8; 32];
        h[..8].copy_from_slice(&i.to_le_bytes());
        FileContent {
            hash: ContentHash(h),
            size: i,
        }
    };
    let name = |s: String| Name::new(&s).expect("Name");

    // Server and local tree with identical content (as after a migration from Synology Drive).
    let mut remote = Vec::with_capacity(n as usize + dirs as usize);
    let mut local = Vec::with_capacity(n as usize + dirs as usize);
    for d in 0..dirs {
        let dn = NodeId(10 + d);
        let dl = LocalId(10 + d);
        remote.push(RemoteChange {
            node: dn,
            state: Some(RemoteEntry {
                parent: root,
                name: name(format!("Ordner {d}")),
                kind: Kind::Dir,
                content: None,
                rev: Rev(1),
            }),
        });
        local.push(LocalObservation {
            id: dl,
            entry: LocalEntry {
                parent: lroot,
                name: name(format!("Ordner {d}")),
                kind: Kind::Dir,
                fp: None,
                content: None,
            },
        });
        for f in 0..per_dir {
            let i = d * per_dir + f;
            remote.push(RemoteChange {
                node: NodeId(10_000_000 + i),
                state: Some(RemoteEntry {
                    parent: dn,
                    name: name(format!("Datei {f}.pdf")),
                    kind: Kind::File,
                    content: Some(content(i)),
                    rev: Rev(1),
                }),
            });
            local.push(LocalObservation {
                id: LocalId(10_000_000 + i),
                entry: LocalEntry {
                    parent: dl,
                    name: name(format!("Datei {f}.pdf")),
                    kind: Kind::File,
                    fp: Some(Fingerprint {
                        size: i,
                        mtime_ns: 1,
                        ctime_ns: 1,
                    }),
                    content: Some(content(i)),
                },
            });
        }
    }

    let mut e = Engine::new(Config {
        remote_root: root,
        device: "Mac".into(),
        local_case_insensitive: true,
    });
    let t = Instant::now();
    e.on_remote_changes(remote, Seq(1));
    println!(
        "{n} Dateien: Server-Stand übernehmen  {:>8.2?}",
        t.elapsed()
    );
    let t = Instant::now();
    e.on_local_snapshot(lroot, local.clone());
    println!(
        "{n} Dateien: lokalen Scan übernehmen  {:>8.2?}",
        t.elapsed()
    );
    let t = Instant::now();
    let mut rounds = 0;
    loop {
        rounds += 1;
        let ops = e.plan();
        assert!(ops.is_empty(), "Ersteinrichtung darf nichts übertragen");
        if rounds > 1 + 1 + (n.ilog10() as usize) {
            break;
        }
    }
    println!(
        "{n} Dateien: verknüpfen ({rounds} Durchläufe) {:>8.2?}",
        t.elapsed()
    );
    let t = Instant::now();
    let ops = e.plan();
    println!(
        "{n} Dateien: Planung im Ruhezustand   {:>8.2?} ({} Ops)",
        t.elapsed(),
        ops.len()
    );
    // A single file is modified locally (as after an FSEvents event).
    let mut changed = local
        .iter()
        .rev()
        .find(|o| o.entry.kind == Kind::File)
        .expect("Datei")
        .clone();
    changed.entry.content = Some(content(u64::MAX / 2));
    changed.entry.fp = Some(Fingerprint {
        size: 1,
        mtime_ns: 2,
        ctime_ns: 2,
    });
    let t = Instant::now();
    e.on_local_changes(vec![changed], vec![]);
    let ops = e.plan();
    println!(
        "{n} Dateien: 1 Änderung erkennen+planen {:>8.2?} ({} Ops)",
        t.elapsed(),
        ops.len()
    );
    let t = Instant::now();
    e.on_local_snapshot(lroot, local);
    let ops = e.plan();
    println!(
        "{n} Dateien: voller Rescan + Planung  {:>8.2?} ({} Ops)",
        t.elapsed(),
        ops.len()
    );
}

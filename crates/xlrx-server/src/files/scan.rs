//! Reconciliation scan (PLAN 4.4): brings the database in line with the files on disk.
//!
//! Changes made outside xlrx (SMB, File Station, Synology Drive, the Photos app …) are found by
//! comparing the tree on disk with the nodes. Identity follows the inode, so a renamed or moved
//! file keeps its node (and with it versions, shares and links). A file replaced by "atomic save"
//! (new inode at the same path) is a content change of the same node. Content is hashed only when
//! the fingerprint (size, mtime, ctime) changed.
//!
//! Safety: a missing root directory never counts as "everything deleted", and a root that is
//! suddenly empty while the database knows many files is refused (unmounted share).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use xlrx_chunk::{Chunker, FileDigest, Fingerprint, RACY_WINDOW_NS, digest_file};

use super::db::{self, NewNode, NodeRow, OnDisk, RootRow, Source};
use super::fs::{Seen, walk};
use crate::error::{ApiError, ApiResult};

/// Above this many known nodes, an empty root directory is treated as an unmounted share.
const EMPTY_ROOT_GUARD: usize = 20;
/// Database changes per transaction (keeps the journal lock short for API writes).
const BATCH: usize = 500;

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanReport {
    pub created: usize,
    pub updated: usize,
    pub moved: usize,
    pub deleted: usize,
    pub hashed_bytes: u64,
    pub skipped: Vec<String>,
}

impl ScanReport {
    pub fn changes(&self) -> usize {
        self.created + self.updated + self.moved + self.deleted
    }
}

/// What the scan decided for one entry on disk.
enum Plan {
    /// Root directory or unchanged; maybe a new identity or fingerprint.
    Keep {
        node: NodeRow,
        disk_changed: bool,
        disk: OnDisk,
    },
    /// Known node; location and/or content changed.
    Change {
        node: NodeRow,
        moved: bool,
        content: Option<([u8; 32], u64)>,
        disk: OnDisk,
    },
    New {
        content: Option<([u8; 32], u64)>,
        disk: OnDisk,
    },
    /// Changed while reading: keep the old state (and the node), the next scan retries.
    Skip { node: Option<NodeRow> },
}

/// Scans a whole root.
pub async fn scan_root(
    db: &sqlx::PgPool,
    data_dir: &Path,
    root: &RootRow,
) -> ApiResult<ScanReport> {
    let dir = data_dir.join(&root.rel_path);
    let known = db::live_nodes(db, root.id).await?;
    let root_node = known
        .iter()
        .find(|n| n.parent_id.is_none())
        .cloned()
        .ok_or_else(|| ApiError::Internal(format!("Ablage {} ohne Wurzelknoten", root.id)))?;

    let dir2 = dir.clone();
    let known2 = known.clone();
    let (seen, plans, skipped, hashed) = tokio::task::spawn_blocking(move || plan(&dir2, &known2))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))??;

    if seen.len() <= 1 && known.len() > EMPTY_ROOT_GUARD {
        return Err(ApiError::Internal(format!(
            "{}: Ordner ist leer, die Datenbank kennt aber {} Einträge. Nicht eingehängt? Abgleich abgebrochen.",
            dir.display(),
            known.len()
        )));
    }

    let mut report = ScanReport {
        skipped,
        hashed_bytes: hashed,
        ..Default::default()
    };
    apply(db, root, &root_node, &known, &seen, plans, &mut report).await?;
    sqlx::query("UPDATE roots SET scanned_at = now() WHERE id = $1")
        .bind(root.id)
        .execute(db)
        .await?;
    if report.changes() > 0 {
        tracing::info!(root = root.id, ?report.created, ?report.updated, ?report.moved, ?report.deleted, "Abgleich");
    }
    Ok(report)
}

type Planned = (Vec<Seen>, Vec<Plan>, Vec<String>, u64);

/// Walks the disk and decides, per entry, what to do (blocking: stat and hashing).
fn plan(dir: &Path, known: &[NodeRow]) -> ApiResult<Planned> {
    let w = walk(dir, Path::new(""), true).map_err(|e| {
        ApiError::Internal(format!(
            "{}: Ablage nicht lesbar ({e}). Abgleich abgebrochen.",
            dir.display()
        ))
    })?;
    let paths = db::paths(known);
    let by_ino: HashMap<(u64, u64), &NodeRow> = known
        .iter()
        .filter_map(|n| n.file_id().map(|f| ((f.dev, f.ino), n)))
        .collect();
    let by_path: HashMap<&Path, &NodeRow> = known
        .iter()
        .filter_map(|n| paths.get(&n.id).map(|p| (p.as_path(), n)))
        .collect();

    // Match every entry on disk to a node: by inode first (moves), then by path (atomic save).
    // Two passes, so that a renamed file keeps its node even if a new file now sits at its old
    // path and is listed first.
    let mut used: HashSet<i64> = HashSet::new();
    let mut matched: Vec<Option<&NodeRow>> = vec![None; w.entries.len()];
    let usable = |n: &NodeRow, s: &Seen, used: &HashSet<i64>| {
        n.is_dir() == s.is_dir && n.parent_id.is_some() && !used.contains(&n.id)
    };
    for (i, s) in w.entries.iter().enumerate() {
        let m = if s.rel.as_os_str().is_empty() {
            known.iter().find(|n| n.parent_id.is_none())
        } else {
            by_ino
                .get(&(s.id.dev, s.id.ino))
                .copied()
                .filter(|n| usable(n, s, &used) && same_file(n, s))
        };
        if let Some(n) = m {
            used.insert(n.id);
            matched[i] = Some(n);
        }
    }
    for (i, s) in w.entries.iter().enumerate() {
        if matched[i].is_some() || s.rel.as_os_str().is_empty() {
            continue;
        }
        if let Some(n) = by_path
            .get(s.rel.as_path())
            .copied()
            .filter(|n| usable(n, s, &used))
        {
            used.insert(n.id);
            matched[i] = Some(n);
        }
    }

    // Moved = different parent node or different name. Children of a moved folder keep their
    // parent and are not moves themselves. Parents are listed before their children.
    let index: HashMap<&Path, usize> = w
        .entries
        .iter()
        .enumerate()
        .map(|(i, s)| (s.rel.as_path(), i))
        .collect();
    let moved: Vec<bool> = w
        .entries
        .iter()
        .zip(&matched)
        .map(|(s, m)| {
            let Some(n) = m.filter(|n| n.parent_id.is_some()) else {
                return false;
            };
            let parent = s
                .parent_rel()
                .and_then(|p| index.get(p))
                .and_then(|&pi| matched[pi])
                .map(|p| p.id);
            parent.is_none() || n.parent_id != parent || n.name != s.name
        })
        .collect();

    let needs_hash: Vec<usize> = (0..w.entries.len())
        .filter(|&i| !w.entries[i].is_dir && stale(matched[i], &w.entries[i], moved[i]))
        .collect();
    let digests: HashMap<usize, Option<Hashed>> = needs_hash
        .par_iter()
        .map_init(Chunker::new, |chunker, &i| {
            let path = dir.join(&w.entries[i].rel);
            let d = match digest_file(chunker, &path) {
                Ok(FileDigest::Stable {
                    digest,
                    fingerprint,
                    ..
                }) if fingerprint == w.entries[i].fp => Some(Hashed {
                    hash: digest.content.hash.0,
                    size: digest.content.size,
                    trusted: !racy(&fingerprint, now_ns()),
                }),
                // Changed while reading or in the meantime: retry with the next scan.
                _ => None,
            };
            (i, d)
        })
        .collect();

    let mut hashed = 0u64;
    let mut skipped = w.skipped;
    let mut plans = Vec::with_capacity(w.entries.len());
    for (i, s) in w.entries.iter().enumerate() {
        let mut disk = OnDisk {
            id: s.id,
            fp: Some(s.fp),
        };
        let content = if s.is_dir {
            None
        } else {
            match digests.get(&i) {
                Some(Some(h)) => {
                    hashed += h.size;
                    if !h.trusted {
                        disk.fp = None;
                    }
                    Some((h.hash, h.size))
                }
                Some(None) => {
                    skipped.push(format!("{}: während des Lesens geändert", s.rel.display()));
                    plans.push(Plan::Skip {
                        node: matched[i].cloned(),
                    });
                    continue;
                }
                None => None, // fingerprint unchanged
            }
        };
        plans.push(match matched[i] {
            None => Plan::New { content, disk },
            Some(n) => {
                let content_changed = content.filter(|c| {
                    n.content_hash.as_deref() != Some(&c.0[..]) || n.size != Some(c.1 as i64)
                });
                if moved[i] || content_changed.is_some() {
                    Plan::Change {
                        node: n.clone(),
                        moved: moved[i],
                        content: content_changed,
                        disk,
                    }
                } else {
                    Plan::Keep {
                        node: n.clone(),
                        disk_changed: n.file_id() != Some(disk.id) || n.fingerprint() != disk.fp,
                        disk,
                    }
                }
            }
        });
    }
    Ok((w.entries, plans, skipped, hashed))
}

struct Hashed {
    hash: [u8; 32],
    size: u64,
    /// The fingerprint may be stored with the hash (see [`racy`]).
    trusted: bool,
}

/// An inode match is only believed for a file whose size and mtime are unchanged (a move keeps
/// both). Otherwise the inode may have been reused by an unrelated new file (ext4 does this right
/// away), which must never inherit the old file's versions and shares. A file moved and edited
/// between two scans therefore becomes delete + create.
fn same_file(n: &NodeRow, s: &Seen) -> bool {
    // The database keeps microseconds.
    let same_mtime = match (n.mtime, s.mtime) {
        (Some(a), Some(b)) => (a - b).abs() < time::Duration::microseconds(1),
        (a, b) => a.is_none() && b.is_none(),
    };
    s.is_dir || (n.size == Some(s.fp.size as i64) && same_mtime)
}

/// Must a file be (re)hashed? Yes if it is new, has no trusted fingerprint, is a different file
/// (atomic save: new inode at the same path) or its fingerprint changed. A rename changes ctime,
/// so files that moved compare only size and mtime.
fn stale(node: Option<&NodeRow>, s: &Seen, moved: bool) -> bool {
    let Some(n) = node else { return true };
    let (Some(fp), Some(_)) = (n.fingerprint(), n.content_hash.as_ref()) else {
        return true;
    };
    n.file_id() != Some(s.id)
        || fp.size != s.fp.size
        || fp.mtime_ns != s.fp.mtime_ns
        || (!moved && fp.ctime_ns != s.fp.ctime_ns)
}

/// Modified so shortly before hashing that a further change with the same timestamp could go
/// unnoticed (as in git). Such hashes are kept, but the next scan reads the file again.
fn racy(fp: &Fingerprint, hashed_at_ns: i64) -> bool {
    fp.mtime_ns.max(fp.ctime_ns) > hashed_at_ns.saturating_sub(RACY_WINDOW_NS)
}

fn now_ns() -> i64 {
    i64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// Writes the decisions in batches. Entries come in breadth-first order, so a parent always has
/// its final node before its children are placed.
async fn apply(
    db: &sqlx::PgPool,
    root: &RootRow,
    root_node: &NodeRow,
    known: &[NodeRow],
    seen: &[Seen],
    plans: Vec<Plan>,
    report: &mut ScanReport,
) -> ApiResult<()> {
    let mut node_of_path: HashMap<PathBuf, i64> = HashMap::new();
    node_of_path.insert(PathBuf::new(), root_node.id);
    let mut alive: HashSet<i64> = HashSet::new();
    alive.insert(root_node.id);
    let mut tx = db::begin_write(db).await?;
    let mut ops = 0usize;
    for (s, p) in seen.iter().zip(plans) {
        let parent = s.parent_rel().and_then(|pr| node_of_path.get(pr).copied());
        match p {
            Plan::Skip { node } => {
                // Keep the known node alive so it is not deleted.
                if let Some(n) = node {
                    alive.insert(n.id);
                    node_of_path.insert(s.rel.clone(), n.id);
                }
                continue;
            }
            Plan::Keep {
                node,
                disk_changed,
                disk,
            } => {
                alive.insert(node.id);
                node_of_path.insert(s.rel.clone(), node.id);
                if disk_changed {
                    db::set_disk(&mut tx, node.id, disk, s.mtime).await?;
                    ops += 1;
                }
            }
            Plan::Change {
                node,
                moved,
                content,
                disk,
            } => {
                alive.insert(node.id);
                node_of_path.insert(s.rel.clone(), node.id);
                if moved {
                    let Some(parent) = parent else { continue };
                    db::set_location(&mut tx, &node, parent, &s.name, Source::SCAN).await?;
                    report.moved += 1;
                    ops += 1;
                }
                match content {
                    Some((hash, size)) => {
                        db::set_content(
                            &mut tx,
                            &node,
                            hash,
                            size,
                            s.mtime,
                            Some(disk),
                            Source::SCAN,
                        )
                        .await?;
                        report.updated += 1;
                    }
                    None => db::set_disk(&mut tx, node.id, disk, s.mtime).await?,
                }
                ops += 1;
            }
            Plan::New { content, disk } => {
                let Some(parent) = parent else { continue };
                let (id, _) = db::insert_node(
                    &mut tx,
                    &NewNode {
                        root_id: root.id,
                        parent_id: Some(parent),
                        name: &s.name,
                        is_dir: s.is_dir,
                        content,
                        mtime: s.mtime,
                        disk: Some(disk),
                    },
                    Source::SCAN,
                )
                .await?;
                alive.insert(id);
                node_of_path.insert(s.rel.clone(), id);
                report.created += 1;
                ops += 1;
            }
        }
        if ops >= BATCH {
            tx.commit().await?;
            tx = db::begin_write(db).await?;
            ops = 0;
        }
    }
    // Everything the database knows but the disk no longer has was deleted outside xlrx.
    for n in known.iter().filter(|n| !alive.contains(&n.id)) {
        db::mark_deleted(&mut tx, n, None, Source::SCAN).await?;
        report.deleted += 1;
        ops += 1;
        if ops >= BATCH {
            tx.commit().await?;
            tx = db::begin_write(db).await?;
            ops = 0;
        }
    }
    tx.commit().await?;
    Ok(())
}

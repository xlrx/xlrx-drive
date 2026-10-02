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

/// What one scan looks at: entries on disk (parents before children) and what the database knows
/// about them, with each known node's path relative to the root directory.
struct Scope {
    entries: Vec<Seen>,
    skipped: Vec<String>,
    known: Vec<NodeRow>,
    paths: HashMap<i64, PathBuf>,
    /// Leave new or changed files alone while they are still being written (timestamps younger
    /// than the racy window): partial scans, so clients never see half-copied files.
    settle: bool,
}

/// Scans a whole root.
pub async fn scan_root(
    db: &sqlx::PgPool,
    data_dir: &Path,
    root: &RootRow,
) -> ApiResult<ScanReport> {
    let dir = data_dir.join(&root.rel_path);
    let known = db::live_nodes(db, root.id).await?;
    if !known.iter().any(|n| n.parent_id.is_none()) {
        return Err(ApiError::Internal(format!(
            "Ablage {} ohne Wurzelknoten",
            root.id
        )));
    }
    let paths = db::paths(&known);
    let d = dir.clone();
    let w = tokio::task::spawn_blocking(move || walk(&d, Path::new(""), true))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| {
            ApiError::Internal(format!(
                "{}: Ablage nicht lesbar ({e}). Abgleich abgebrochen.",
                dir.display()
            ))
        })?;
    if w.entries.len() <= 1 && known.len() > EMPTY_ROOT_GUARD {
        return Err(ApiError::Internal(format!(
            "{}: Ordner ist leer, die Datenbank kennt aber {} Einträge. Nicht eingehängt? Abgleich abgebrochen.",
            dir.display(),
            known.len()
        )));
    }
    let scope = Scope {
        entries: w.entries,
        skipped: w.skipped,
        known,
        paths,
        settle: false,
    };
    let (report, _, _) = run(db, &dir, root, scope, Deletion::All).await?;
    sqlx::query("UPDATE roots SET scanned_at = now() WHERE id = $1")
        .bind(root.id)
        .execute(db)
        .await?;
    Ok(report)
}

/// Which missing nodes a scan may mark as deleted.
enum Deletion {
    /// Full scan: everything the disk no longer has.
    All,
    /// Partial scan: only in these folders; elsewhere the folders are reported back (a move may
    /// still be on its way, see [`scan_dirs`]).
    In(HashSet<PathBuf>),
}

/// Result of [`scan_dirs`].
#[derive(Debug, Default)]
pub struct PartialReport {
    pub report: ScanReport,
    /// Folders with missing entries that were not marked deleted yet.
    pub missing_in: Vec<PathBuf>,
    /// Folders not known (yet) under that path; their nearest known ancestor was scanned instead.
    pub unresolved: Vec<PathBuf>,
    /// Folders with files still being written: to be scanned again shortly.
    pub busy: Vec<PathBuf>,
}

/// Scans only the given folders (relative to the root directory): their entries, and new
/// subfolders completely. Used by the watcher, so a change shows up in seconds without walking
/// the whole root. Entries that moved in from elsewhere keep their node (found by inode).
/// Missing entries are marked deleted only in the folders of `delete_in`; elsewhere they are
/// reported, so a move whose two halves arrive in different batches is not taken for a deletion.
pub async fn scan_dirs(
    db: &sqlx::PgPool,
    data_dir: &Path,
    root: &RootRow,
    dirs: &[PathBuf],
    delete_in: &HashSet<PathBuf>,
) -> ApiResult<PartialReport> {
    let root_dir = data_dir.join(&root.rel_path);
    let mut out = PartialReport::default();

    // Folders as the database knows them; an unknown one is replaced by its nearest known ancestor.
    let mut anchors: Vec<(PathBuf, NodeRow)> = Vec::new();
    let mut wanted: Vec<PathBuf> = dirs.to_vec();
    wanted.sort_by_key(|d| d.components().count());
    wanted.dedup();
    for d in wanted {
        let mut cur = d.clone();
        loop {
            if anchors.iter().any(|(r, _)| *r == cur) {
                break;
            }
            if let Some(n) = db::node_at_path(db, root.id, &cur)
                .await?
                .filter(NodeRow::is_dir)
            {
                anchors.push((cur.clone(), n));
                break;
            }
            if cur == d {
                out.unresolved.push(d.clone());
            }
            match cur.parent() {
                Some(p) => cur = p.to_path_buf(),
                None => break,
            }
        }
    }
    anchors.sort_by_key(|(r, _)| r.components().count());

    // List the folders (and nothing below them yet).
    let rd = root_dir.clone();
    let rels: Vec<PathBuf> = anchors.iter().map(|(r, _)| r.clone()).collect();
    let (mut entries, mut skipped, listed) = tokio::task::spawn_blocking(move || {
        let mut entries: Vec<Seen> = Vec::new();
        let mut skipped = Vec::new();
        let mut listed = Vec::new();
        let mut have: HashSet<PathBuf> = HashSet::new();
        for rel in rels {
            let w = match walk(&rd, &rel, false) {
                Ok(w) => w,
                // Gone meanwhile: its parent's events will follow.
                Err(_) => continue,
            };
            listed.push(rel);
            skipped.extend(w.skipped);
            for e in w.entries {
                if have.insert(e.rel.clone()) {
                    entries.push(e);
                }
            }
        }
        (entries, skipped, listed)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    anchors.retain(|(r, _)| listed.contains(r));
    if anchors.is_empty() {
        return Ok(out);
    }

    // What the database knows: the folders, their children, and nodes that moved in from
    // elsewhere (same identity on disk).
    let mut known: Vec<NodeRow> = anchors.iter().map(|(_, n)| n.clone()).collect();
    let mut paths: HashMap<i64, PathBuf> = anchors.iter().map(|(r, n)| (n.id, r.clone())).collect();
    let anchor_ids: Vec<i64> = anchors.iter().map(|(_, n)| n.id).collect();
    let anchor_path: HashMap<i64, PathBuf> =
        anchors.iter().map(|(r, n)| (n.id, r.clone())).collect();
    for c in db::children_of(db, &anchor_ids).await? {
        if paths.contains_key(&c.id) {
            continue;
        }
        let parent = c
            .parent_id
            .and_then(|p| anchor_path.get(&p))
            .cloned()
            .unwrap_or_default();
        paths.insert(c.id, parent.join(&c.name));
        known.push(c);
    }
    let known_ids: HashSet<(u64, u64)> = known
        .iter()
        .filter_map(|n| n.file_id().map(|f| (f.dev, f.ino)))
        .collect();
    let foreign: Vec<xlrx_chunk::FileId> = entries
        .iter()
        .filter(|e| !known_ids.contains(&(e.id.dev, e.id.ino)))
        .map(|e| e.id)
        .collect();
    for n in db::by_file_ids(db, root.id, &foreign).await? {
        if paths.contains_key(&n.id) || n.parent_id.is_none() {
            continue;
        }
        paths.insert(n.id, db::rel_path(db, n.id).await?);
        known.push(n);
    }

    // New folders are read completely (their content is new as well).
    let known_ids: HashSet<(u64, u64)> = known
        .iter()
        .filter_map(|n| n.file_id().map(|f| (f.dev, f.ino)))
        .collect();
    let known_paths: HashSet<&Path> = paths.values().map(PathBuf::as_path).collect();
    let new_dirs: Vec<PathBuf> = entries
        .iter()
        .filter(|e| {
            e.is_dir
                && !known_ids.contains(&(e.id.dev, e.id.ino))
                && !known_paths.contains(e.rel.as_path())
        })
        .map(|e| e.rel.clone())
        .collect();
    if !new_dirs.is_empty() {
        let rd = root_dir.clone();
        let more = tokio::task::spawn_blocking(move || {
            let mut more = Vec::new();
            let mut skipped = Vec::new();
            for d in new_dirs {
                if let Ok(w) = walk(&rd, &d, true) {
                    // The first entry is the folder itself, already listed.
                    more.extend(w.entries.into_iter().skip(1));
                    skipped.extend(w.skipped);
                }
            }
            (more, skipped)
        })
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?;
        entries.extend(more.0);
        skipped.extend(more.1);
    }

    let scope = Scope {
        entries,
        skipped,
        known,
        paths,
        settle: true,
    };
    let (report, missing_in, busy) =
        run(db, &root_dir, root, scope, Deletion::In(delete_in.clone())).await?;
    out.report = report;
    out.missing_in = missing_in;
    out.busy = busy;
    Ok(out)
}

/// Plans (blocking: stat and hashing) and applies a scan.
async fn run(
    db: &sqlx::PgPool,
    dir: &Path,
    root: &RootRow,
    scope: Scope,
    deletion: Deletion,
) -> ApiResult<(ScanReport, Vec<PathBuf>, Vec<PathBuf>)> {
    let d = dir.to_path_buf();
    let (scope, (plans, skipped, hashed, busy)) = tokio::task::spawn_blocking(move || {
        let planned = plan(&d, &scope);
        (scope, planned)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut report = ScanReport {
        skipped,
        hashed_bytes: hashed,
        ..Default::default()
    };
    let missing_in = apply(db, root, &scope, plans, deletion, &mut report).await?;
    if report.changes() > 0 {
        tracing::info!(root = root.id, ?report.created, ?report.updated, ?report.moved, ?report.deleted, "Abgleich");
    }
    Ok((report, missing_in, busy))
}

/// Decides, per entry, what to do. Also returns the folders with files still being written
/// (only with [`Scope::settle`]).
fn plan(dir: &Path, scope: &Scope) -> (Vec<Plan>, Vec<String>, u64, Vec<PathBuf>) {
    let (entries, known, paths) = (&scope.entries, &scope.known, &scope.paths);
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
    let mut matched: Vec<Option<&NodeRow>> = vec![None; entries.len()];
    let usable = |n: &NodeRow, s: &Seen, used: &HashSet<i64>| {
        n.is_dir() == s.is_dir && n.parent_id.is_some() && !used.contains(&n.id)
    };
    for (i, s) in entries.iter().enumerate() {
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
    for (i, s) in entries.iter().enumerate() {
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
    let index: HashMap<&Path, usize> = entries
        .iter()
        .enumerate()
        .map(|(i, s)| (s.rel.as_path(), i))
        .collect();
    let moved: Vec<bool> = entries
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
            match s.parent_rel().map(|p| index.contains_key(p)) {
                // The parent folder is part of this scan: compare with its node.
                Some(true) => parent.is_none() || n.parent_id != parent || n.name != s.name,
                // A folder a partial scan starts from: compare paths.
                _ => paths.get(&n.id).map(PathBuf::as_path) != Some(s.rel.as_path()),
            }
        })
        .collect();

    let mut needs_hash: Vec<usize> = (0..entries.len())
        .filter(|&i| !entries[i].is_dir && stale(matched[i], &entries[i], moved[i]))
        .collect();
    // Still being written: not now (partial scans only; a full scan records what it finds).
    let now = now_ns();
    let settling: HashSet<usize> = if scope.settle {
        needs_hash
            .iter()
            .copied()
            .filter(|&i| racy(&entries[i].fp, now))
            .collect()
    } else {
        HashSet::new()
    };
    needs_hash.retain(|i| !settling.contains(i));
    let mut busy: Vec<PathBuf> = Vec::new();
    let digests: HashMap<usize, Option<Hashed>> = needs_hash
        .par_iter()
        .map_init(Chunker::new, |chunker, &i| {
            let path = dir.join(&entries[i].rel);
            let d = match digest_file(chunker, &path) {
                Ok(FileDigest::Stable {
                    digest,
                    fingerprint,
                    ..
                }) if fingerprint == entries[i].fp => Some(Hashed {
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
    let mut skipped = scope.skipped.clone();
    let mut plans = Vec::with_capacity(entries.len());
    for (i, s) in entries.iter().enumerate() {
        if settling.contains(&i) {
            let folder = s.parent_rel().unwrap_or(Path::new("")).to_path_buf();
            if !busy.contains(&folder) {
                busy.push(folder);
            }
            plans.push(Plan::Skip {
                node: matched[i].cloned(),
            });
            continue;
        }
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
    (plans, skipped, hashed, busy)
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
/// its final node before its children are placed. Returns the folders with missing entries that
/// were not marked deleted (see [`Deletion`]).
async fn apply(
    db: &sqlx::PgPool,
    root: &RootRow,
    scope: &Scope,
    plans: Vec<Plan>,
    deletion: Deletion,
    report: &mut ScanReport,
) -> ApiResult<Vec<PathBuf>> {
    let mut node_of_path: HashMap<PathBuf, i64> = HashMap::new();
    let mut alive: HashSet<i64> = HashSet::new();
    let mut tx = db::begin_write(db).await?;
    let mut ops = 0usize;
    for (s, p) in scope.entries.iter().zip(plans) {
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
    // What the database knows but the disk no longer has was deleted outside xlrx. A folder takes
    // everything below it along.
    let missing: Vec<&NodeRow> = scope
        .known
        .iter()
        .filter(|n| n.parent_id.is_some() && !alive.contains(&n.id))
        .collect();
    let missing_ids: HashSet<i64> = missing.iter().map(|n| n.id).collect();
    let mut kept_in: Vec<PathBuf> = Vec::new();
    for n in missing {
        if n.parent_id.is_some_and(|p| missing_ids.contains(&p)) {
            continue;
        }
        let folder = scope
            .paths
            .get(&n.id)
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_default();
        let now = match &deletion {
            Deletion::All => true,
            Deletion::In(dirs) => dirs.contains(&folder),
        };
        if !now {
            if !kept_in.contains(&folder) {
                kept_in.push(folder);
            }
            continue;
        }
        report.deleted += db::mark_deleted_subtree(&mut tx, root.id, n.id).await? as usize;
        ops += 1;
        if ops >= BATCH {
            tx.commit().await?;
            tx = db::begin_write(db).await?;
            ops = 0;
        }
    }
    tx.commit().await?;
    Ok(kept_in)
}

//! Notices changes made outside xlrx (SMB, File Station, Synology Drive, the Photos app …) within
//! seconds (PLAN 4.4): inotify on every folder of a root; events are collected until things are
//! quiet for a moment, then only the affected folders are scanned ([`roots::scan_dirs`]).
//!
//! Missing entries are marked deleted only after a grace period (the other half of a move may
//! still be on its way), and files still being written are picked up once they are quiet. If
//! the kernel's event queue overflows, or anything looks inconsistent, a full scan settles it;
//! the hourly full scan stays as the safety net.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::db::RootRow;
use super::fs::ignored;
use super::roots;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Quiet period before a batch of changes is scanned.
const QUIET: Duration = Duration::from_millis(400);
/// A batch is scanned at the latest this long after its first event (continuous activity).
const MAX_WAIT: Duration = Duration::from_secs(3);
/// Missing entries are marked deleted after this long; files being written are looked at again.
const GRACE: Duration = Duration::from_secs(2);
/// A folder not known under its path is retried this often, then a full scan settles it.
const RETRIES: u8 = 3;

enum Msg {
    /// Something in this folder (relative to the root directory) changed.
    Changed(PathBuf),
    /// Events were lost: scan everything.
    Rescan,
}

/// Starts watching a root (does nothing if it is watched already).
pub async fn start(st: &AppState, root: &RootRow) -> ApiResult<()> {
    if st.watching(root.id) {
        return Ok(());
    }
    let data_dir = st
        .cfg
        .data_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))?;
    let dir = roots::dir(&data_dir, root);
    let (tx, rx) = mpsc::unbounded_channel();
    let base = dir.clone();
    let watcher = notify::recommended_watcher(move |res: notify::Result<Event>| match res {
        Ok(ev) if ev.need_rescan() => {
            let _ = tx.send(Msg::Rescan);
        }
        Ok(ev) if relevant(&ev.kind) => {
            for p in &ev.paths {
                if let Some(folder) = folder_of(&base, p) {
                    let _ = tx.send(Msg::Changed(folder));
                }
            }
        }
        Ok(_) => {}
        Err(_) => {
            let _ = tx.send(Msg::Rescan);
        }
    })
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    // One inotify watch per folder: takes a moment on a large root.
    let watcher = tokio::task::spawn_blocking(move || {
        let mut w = watcher;
        w.watch(&dir, RecursiveMode::Recursive).map(|()| w)
    })
    .await
    .map_err(|e| ApiError::Internal(e.to_string()))?
    .map_err(|e| {
        if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
            ApiError::Internal(
                "Zu wenige inotify-Watches: fs.inotify.max_user_watches erhöhen (deploy/synology.md). \
                 Änderungen von außen erscheinen bis dahin erst mit dem stündlichen Abgleich."
                    .into(),
            )
        } else {
            ApiError::Internal(format!("Überwachung von Ablage {}: {e}", root.id))
        }
    })?;
    if !st.keep_watcher(root.id, watcher) {
        return Ok(());
    }
    tracing::info!(root = root.id, "Ablage wird überwacht");
    tokio::spawn(run(st.clone(), root.clone(), rx));
    Ok(())
}

/// Events that can change what a scan sees. Opening and reading (also by xlrx's own scans) and
/// writes in progress are not; a finished write is (`IN_CLOSE_WRITE`).
fn relevant(kind: &EventKind) -> bool {
    match kind {
        EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        EventKind::Access(_) => false,
        EventKind::Modify(ModifyKind::Data(_)) => false,
        _ => true,
    }
}

/// The folder an event touched, relative to the root directory; `None` for names that are never
/// synced (Synology's `@eaDir` thumbnails, our own temp files …).
fn folder_of(base: &Path, path: &Path) -> Option<PathBuf> {
    let rel = path.strip_prefix(base).ok()?;
    let skip = rel
        .components()
        .any(|c| matches!(c, Component::Normal(n) if ignored(&n.to_string_lossy())));
    if skip {
        return None;
    }
    Some(rel.parent().map(Path::to_path_buf).unwrap_or_default())
}

/// A folder to look at again later.
#[derive(Clone, Copy)]
struct Later {
    at: Instant,
    /// Missing entries may be marked deleted then.
    delete: bool,
}

async fn run(st: AppState, root: RootRow, mut rx: mpsc::UnboundedReceiver<Msg>) {
    let mut dirs: HashSet<PathBuf> = HashSet::new();
    let mut later: HashMap<PathBuf, Later> = HashMap::new();
    let mut retries: HashMap<PathBuf, u8> = HashMap::new();
    let mut full = false;
    let mut first: Option<Instant> = None;
    let mut last = Instant::now();
    loop {
        let batch_at = first.map(|f| (last + QUIET).min(f + MAX_WAIT));
        let later_at = later.values().map(|l| l.at).min();
        let due = match (batch_at, later_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        tokio::select! {
            msg = rx.recv() => {
                let now = Instant::now();
                match msg {
                    // The watcher was dropped: the root is no longer watched.
                    None => return,
                    Some(Msg::Changed(d)) => {
                        dirs.insert(d);
                    }
                    Some(Msg::Rescan) => full = true,
                }
                first.get_or_insert(now);
                last = now;
            }
            () = sleep_until(due), if due.is_some() => {
                let now = Instant::now();
                let batch = batch_at.is_some_and(|t| t <= now);
                let expired: Vec<(PathBuf, Later)> = later
                    .iter()
                    .filter(|(_, l)| l.at <= now)
                    .map(|(d, l)| (d.clone(), *l))
                    .collect();
                if !batch && expired.is_empty() {
                    continue;
                }
                if full {
                    full = false;
                    first = None;
                    dirs.clear();
                    later.clear();
                    retries.clear();
                    if let Err(e) = roots::scan(&st, &root).await {
                        tracing::warn!(root = root.id, error = ?e, "Abgleich fehlgeschlagen");
                    }
                    continue;
                }
                let mut scan: Vec<PathBuf> = Vec::new();
                if batch {
                    first = None;
                    scan.extend(dirs.drain());
                }
                let mut delete_in = HashSet::new();
                for (d, l) in expired {
                    later.remove(&d);
                    if l.delete {
                        delete_in.insert(d.clone());
                    }
                    if !scan.contains(&d) {
                        scan.push(d);
                    }
                }
                match roots::scan_dirs(&st, &root, &scan, &delete_in).await {
                    Ok(r) => {
                        let at = Instant::now() + GRACE;
                        for d in r.missing_in {
                            later.entry(d).or_insert(Later { at, delete: true }).delete = true;
                        }
                        for d in r.busy {
                            later.entry(d).or_insert(Later { at, delete: false });
                        }
                        retries.retain(|d, _| r.unresolved.contains(d));
                        for d in r.unresolved {
                            let n = retries.entry(d.clone()).or_insert(0);
                            *n += 1;
                            if *n > RETRIES {
                                full = true;
                            } else {
                                dirs.insert(d);
                            }
                            first.get_or_insert(Instant::now());
                        }
                    }
                    Err(e) => {
                        tracing::warn!(root = root.id, error = ?e, "Teil-Abgleich fehlgeschlagen, ganzer Abgleich folgt");
                        full = true;
                        first.get_or_insert(Instant::now());
                    }
                }
            }
        }
    }
}

async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, RemoveKind, RenameMode};

    #[test]
    fn reads_and_writes_in_progress_are_not_changes() {
        assert!(!relevant(&EventKind::Access(AccessKind::Open(
            AccessMode::Any
        ))));
        assert!(!relevant(&EventKind::Access(AccessKind::Close(
            AccessMode::Read
        ))));
        assert!(!relevant(&EventKind::Modify(ModifyKind::Data(
            notify::event::DataChange::Any
        ))));
        assert!(relevant(&EventKind::Access(AccessKind::Close(
            AccessMode::Write
        ))));
        assert!(relevant(&EventKind::Create(CreateKind::File)));
        assert!(relevant(&EventKind::Remove(RemoveKind::Folder)));
        assert!(relevant(&EventKind::Modify(ModifyKind::Name(
            RenameMode::Both
        ))));
    }

    #[test]
    fn events_map_to_their_folder() {
        let base = Path::new("/vol/homes/k/Drive");
        assert_eq!(
            folder_of(base, Path::new("/vol/homes/k/Drive/a.txt")),
            Some(PathBuf::new())
        );
        assert_eq!(
            folder_of(base, Path::new("/vol/homes/k/Drive/Fotos/b.jpg")),
            Some(PathBuf::from("Fotos"))
        );
        assert_eq!(
            folder_of(
                base,
                Path::new("/vol/homes/k/Drive/Fotos/@eaDir/b.jpg/SYNO.jpg")
            ),
            None
        );
        assert_eq!(
            folder_of(base, Path::new("/vol/homes/k/Drive/.xlrx-srv-12")),
            None
        );
        assert_eq!(folder_of(base, Path::new("/elsewhere/x")), None);
    }
}

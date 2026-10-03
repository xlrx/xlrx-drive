//! Writing file content through xlrx (PLAN 4.2, 4.3): uploads and restored versions.
//!
//! 1. The upload is streamed into `xlrx-state/store/staging` and hashed.
//! 2. Under the root's lock: a replaced file must still be exactly what the database knows.
//!    Otherwise it changed outside xlrx: the root is scanned and the upload refused (409), so the
//!    other change is never lost.
//! 3. The replaced content is cloned into the content-addressed version store.
//! 4. The new content is cloned next to the target under a server temp name and swapped in
//!    atomically (RENAME_EXCHANGE). The swapped-out file is checked once more: if it is not
//!    exactly the version just kept (changed in that very instant), the swap is undone.
//! 5. Node, version and journal are committed; staging and the old file are removed.
//!
//! Steps 3–5 are announced in `pending_ops`; [`recover_upload`] cleans up after a crash.

use std::path::{Path, PathBuf};

use axum::body::Body;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use xlrx_chunk::{Chunker, FileDigest, Fingerprint, digest_file, fingerprint_of};

use super::db::{self, NodeRow, OnDisk, Source};
use super::ops::{
    Intent, Place, blocking, ensure_free, forget, io_err, live, located, place, record, reload,
    valid_name, writable,
};
use super::{roots, scan, store};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use xlrx_sync::Reject;

pub(crate) fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(s: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    if s.len() != 64 {
        return None;
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A fully received upload in the staging area.
pub struct Staged {
    path: PathBuf,
    hash: [u8; 32],
    size: u64,
}

impl Drop for Staged {
    fn drop(&mut self) {
        // Whatever happens to the upload, the staging file goes (crash leftovers: housekeeping).
        if !self.path.as_os_str().is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Staged {
    /// The content's hash.
    pub fn hash(&self) -> &[u8; 32] {
        &self.hash
    }

    /// Where the staged copy lies (read only; it goes when this is dropped).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Keeps the staged file under another name in the staging area (replacing an older one).
    pub fn keep_as(mut self, to: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, to)?;
        self.path = PathBuf::new();
        Ok(())
    }
}

fn state_dir(st: &AppState) -> ApiResult<PathBuf> {
    st.cfg
        .state_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))
}

fn disk_full(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(rustix::io::Errno::NOSPC.raw_os_error())
}

/// Streams a request body into a new staging file and hashes it. With `size`, the body must have
/// exactly that length (an aborted upload never counts as complete).
pub async fn stage(st: &AppState, body: Body, size: Option<u64>) -> ApiResult<Staged> {
    let dir = state_dir(st)?;
    let path = dir
        .join(store::STAGING)
        .join(uuid::Uuid::new_v4().simple().to_string());
    let d = dir.clone();
    blocking(move || store::init(&d)).await?.map_err(io_err)?;
    let mut file = tokio::fs::File::create_new(&path).await.map_err(io_err)?;
    // From here on, dropping `staged` removes the file.
    let mut staged = Staged {
        path,
        hash: [0; 32],
        size: 0,
    };
    let mut stream = body.into_data_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::bad("Upload abgebrochen."))?;
        total += chunk.len() as u64;
        if size.is_some_and(|s| total > s) {
            return Err(ApiError::bad("Upload ist größer als angekündigt."));
        }
        file.write_all(&chunk).await.map_err(|e| {
            if disk_full(&e) {
                ApiError::Conflict("Kein Platz mehr auf dem NAS.".into())
            } else {
                io_err(e)
            }
        })?;
    }
    if size.is_some_and(|s| total != s) {
        return Err(ApiError::bad("Upload unvollständig."));
    }
    file.sync_all().await.map_err(io_err)?;
    drop(file);
    hash_staged(&mut staged).await?;
    Ok(staged)
}

async fn hash_staged(staged: &mut Staged) -> ApiResult<()> {
    let p = staged.path.clone();
    let digest = blocking(move || digest_file(&mut Chunker::new(), &p))
        .await?
        .map_err(io_err)?;
    let FileDigest::Stable { digest, .. } = digest else {
        return Err(ApiError::Internal(
            "Staging-Datei während des Lesens geändert".into(),
        ));
    };
    staged.hash = digest.content.hash.0;
    staged.size = digest.content.size;
    Ok(())
}

/// Takes over a complete file in the state directory (an upload put together from parts) as
/// staged content and hashes it. From now on the file belongs to the returned `Staged`.
pub async fn adopt(path: PathBuf) -> ApiResult<Staged> {
    let mut staged = Staged {
        path,
        hash: [0; 32],
        size: 0,
    };
    hash_staged(&mut staged).await?;
    Ok(staged)
}

/// Copies (reflink where possible) a file into the staging area, but only if its content really
/// is `hash` (the file could have changed since the database recorded it).
pub async fn stage_from(st: &AppState, src: &Path, hash: &[u8; 32]) -> ApiResult<Option<Staged>> {
    let dir = state_dir(st)?;
    let path = dir
        .join(store::STAGING)
        .join(uuid::Uuid::new_v4().simple().to_string());
    let (s, p, d) = (src.to_path_buf(), path.clone(), dir.clone());
    let digest = blocking(move || -> std::io::Result<Option<FileDigest>> {
        store::init(&d)?;
        match store::clone_file(&s, &p) {
            Ok(_) => Ok(Some(digest_file(&mut Chunker::new(), &p)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    })
    .await?
    .map_err(io_err)?;
    let mut staged = Staged {
        path,
        hash: [0; 32],
        size: 0,
    };
    match digest {
        Some(FileDigest::Stable { digest, .. }) if digest.content.hash.0 == *hash => {
            staged.hash = digest.content.hash.0;
            staged.size = digest.content.size;
            Ok(Some(staged))
        }
        // Missing or different: `staged` is dropped and its file removed.
        _ => Ok(None),
    }
}

/// The replaced content of a file, kept as a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OldVersion {
    pub node_id: i64,
    pub rev: i64,
    pub hash: String,
    pub size: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub mtime: Option<OffsetDateTime>,
    /// Relative to the state directory.
    pub store_path: String,
}

/// Where new content goes.
pub enum Target {
    /// A new file in a folder; with `keep_both`, under "Name (1).ext" etc. if the name is taken.
    New {
        parent_id: i64,
        name: String,
        keep_both: bool,
    },
    /// New content for an existing file, only if it is still at revision `base_rev`.
    Replace { node_id: i64, base_rev: i64 },
    /// Sync clients (PLAN 5.3): like `Replace`, but if the file changed meanwhile, the content is
    /// stored next to it as a conflict copy named after `device`. Nothing is overwritten.
    ReplaceOrConflict {
        node_id: i64,
        base_rev: i64,
        device: String,
    },
}

/// What [`write`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Written {
    Created,
    Replaced,
    /// Stored as a conflict copy next to the file (`ReplaceOrConflict`).
    ConflictCopy,
    /// The file already has exactly this content: nothing to do.
    Unchanged,
}

/// Writes staged content into a root (see the module docs). Returns the node and whether it was
/// created.
pub async fn write(
    st: &AppState,
    user_id: i64,
    target: Target,
    staged: Staged,
    mtime: Option<OffsetDateTime>,
) -> ApiResult<(NodeRow, Written)> {
    let any_node = match &target {
        Target::New { parent_id, .. } => *parent_id,
        Target::Replace { node_id, .. } | Target::ReplaceOrConflict { node_id, .. } => *node_id,
    };
    let (_, root) = writable(st, user_id, any_node).await?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let (_, root) = writable(st, user_id, any_node).await?;
    let p = place(st, root)?;
    match target {
        Target::New {
            parent_id,
            name,
            keep_both,
        } => {
            let name = valid_name(&name)?;
            let parent = live(reload(st, parent_id).await?)?;
            if !parent.is_dir() {
                return Err(ApiError::Rejected(
                    Reject::ParentGone,
                    "Kein Ordner.".into(),
                ));
            }
            let dir = located(st, &p, &parent).await?;
            let name = if keep_both {
                free_name(st, &parent, &dir, &name, "").await?
            } else {
                ensure_free(st, &parent, &dir, &name, None).await?;
                name
            };
            let node = write_new(st, &p, user_id, &parent, &dir, &name, &staged, mtime).await?;
            Ok((node, Written::Created))
        }
        Target::Replace { node_id, base_rev } => {
            let node = live(reload(st, node_id).await?)?;
            if node.is_dir() {
                return Err(ApiError::bad("Ordner haben keinen Inhalt."));
            }
            changed_since(&node, base_rev)?;
            let path = located(st, &p, &node).await?;
            let node = write_replace(st, &p, user_id, node, &path, &staged, mtime).await?;
            Ok((node, Written::Replaced))
        }
        Target::ReplaceOrConflict {
            node_id,
            base_rev,
            device,
        } => {
            let node = reload(st, node_id)
                .await?
                .filter_live()
                .filter(|n| !n.is_dir())
                .ok_or_else(|| {
                    ApiError::Rejected(Reject::NodeGone, "Die Datei gibt es nicht mehr.".into())
                })?;
            if node.content_hash.as_deref() == Some(&staged.hash[..])
                && node.size == Some(staged.size as i64)
            {
                // Already exactly this content (also: a repeated request whose answer was lost).
                return Ok((node, Written::Unchanged));
            }
            if node.rev == base_rev {
                let path = located(st, &p, &node).await?;
                let node = write_replace(st, &p, user_id, node, &path, &staged, mtime).await?;
                return Ok((node, Written::Replaced));
            }
            let parent = live(reload(st, node.parent_id.unwrap_or_default()).await?)?;
            let dir = located(st, &p, &parent).await?;
            let date = OffsetDateTime::now_utc()
                .format(CONFLICT_DATE)
                .unwrap_or_default();
            let label = format!("Konflikt – {device} {date}");
            let name = free_name(st, &parent, &dir, &node.name, &label).await?;
            let copy = write_new(st, &p, user_id, &parent, &dir, &name, &staged, mtime).await?;
            Ok((copy, Written::ConflictCopy))
        }
    }
}

const CONFLICT_DATE: &[time::format_description::FormatItem<'static>] =
    time::macros::format_description!("[year]-[month]-[day] [hour].[minute]");

trait FilterLive {
    fn filter_live(self) -> Option<NodeRow>;
}

impl FilterLive for NodeRow {
    fn filter_live(self) -> Option<NodeRow> {
        self.deleted_at.is_none().then_some(self)
    }
}

fn changed_since(node: &NodeRow, base_rev: i64) -> ApiResult<()> {
    if node.rev != base_rev {
        return Err(ApiError::Conflict(format!(
            "„{}“ wurde inzwischen geändert. Bitte neu laden; beide Fassungen bleiben so erhalten.",
            node.name
        )));
    }
    Ok(())
}

/// Without a label: "Name.ext", else "Name (1).ext", "Name (2).ext", … With a label:
/// "Name (label).ext", else "Name (label 2).ext", …
async fn free_name(
    st: &AppState,
    parent: &NodeRow,
    dir: &Path,
    name: &str,
    label: &str,
) -> ApiResult<String> {
    let base = xlrx_proto::Name::new(name).map_err(|e| ApiError::bad(e.to_string()))?;
    let mut candidate = if label.is_empty() {
        name.to_owned()
    } else {
        base.with_suffix(label).as_str().to_owned()
    };
    for i in 1..1000 {
        if ensure_free(st, parent, dir, &candidate, None).await.is_ok() {
            return Ok(candidate);
        }
        let suffix = if label.is_empty() {
            i.to_string()
        } else {
            format!("{label} {}", i + 1)
        };
        candidate = base.with_suffix(&suffix).as_str().to_owned();
    }
    Err(ApiError::Conflict("Kein freier Name gefunden.".into()))
}

/// Clones the staged content next to the target under a server temp name, with its mtime and
/// (when replacing) the permissions of the old file.
fn prepare_temp(
    staged: &Path,
    temp: &Path,
    mtime: Option<OffsetDateTime>,
    like: Option<&Path>,
) -> std::io::Result<()> {
    store::clone_file(staged, temp)?;
    let f = std::fs::File::options().write(true).open(temp)?;
    if let Some(m) = mtime {
        f.set_modified(m.into())?;
    }
    if let Some(old) = like {
        f.set_permissions(std::fs::symlink_metadata(old)?.permissions())?;
    }
    f.sync_all()
}

#[allow(clippy::too_many_arguments)]
async fn write_new(
    st: &AppState,
    p: &Place,
    user_id: i64,
    parent: &NodeRow,
    dir: &Path,
    name: &str,
    staged: &Staged,
    mtime: Option<OffsetDateTime>,
) -> ApiResult<NodeRow> {
    let target = dir.join(name);
    let temp = dir.join(store::temp_name());
    let op = record(
        st,
        &upload_intent(p, user_id, staged, &temp, &target, None)?,
    )
    .await?;
    let (s, t, g) = (staged.path.clone(), temp.clone(), target.clone());
    let d = dir.to_path_buf();
    let done = blocking(move || {
        prepare_temp(&s, &t, mtime, None)?;
        if let Err(e) = store::rename_noreplace(&t, &g) {
            let _ = std::fs::remove_file(&t);
            return Err(e);
        }
        store::fsync_dir(&d)?;
        std::fs::symlink_metadata(&g)
    })
    .await?;
    let meta = match done {
        Ok(m) => m,
        Err(e) => {
            forget(st, op).await?;
            return Err(match e.kind() {
                std::io::ErrorKind::AlreadyExists => {
                    ApiError::Conflict(format!("„{name}“ gibt es hier schon."))
                }
                _ if disk_full(&e) => ApiError::Conflict("Kein Platz mehr auf dem NAS.".into()),
                _ => io_err(e),
            });
        }
    };
    let (fid, _) = fingerprint_of(&meta);
    let mut tx = db::begin_write(&st.db).await?;
    let (id, _) = db::insert_node(
        &mut tx,
        &db::NewNode {
            root_id: p.root.id,
            parent_id: Some(parent.id),
            name,
            is_dir: false,
            content: Some((staged.hash, staged.size)),
            mtime: meta.modified().ok().map(OffsetDateTime::from),
            // Fingerprint taken right after writing is "racy": the next scan confirms it.
            disk: Some(OnDisk { id: fid, fp: None }),
        },
        Source::api(user_id),
    )
    .await?;
    sqlx::query("DELETE FROM pending_ops WHERE id = $1")
        .bind(op)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    reload(st, id).await
}

fn upload_intent(
    p: &Place,
    user_id: i64,
    staged: &Staged,
    temp: &Path,
    target: &Path,
    old: Option<OldVersion>,
) -> ApiResult<Intent> {
    let rel = |path: &Path, base: &Path| -> ApiResult<PathBuf> {
        Ok(path
            .strip_prefix(base)
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .to_path_buf())
    };
    Ok(Intent::Upload {
        root_id: p.root.id,
        actor: user_id,
        staging: rel(&staged.path, &p.state_dir)?,
        temp: rel(temp, &p.data_dir)?,
        target: rel(target, &p.data_dir)?,
        hash: hex(&staged.hash),
        old,
    })
}

/// Fingerprint and identity of a path.
fn stat(path: &Path) -> std::io::Result<(xlrx_chunk::FileId, Fingerprint)> {
    Ok(fingerprint_of(&std::fs::symlink_metadata(path)?))
}

#[allow(clippy::too_many_arguments)]
async fn write_replace(
    st: &AppState,
    p: &Place,
    user_id: i64,
    node: NodeRow,
    path: &Path,
    staged: &Staged,
    mtime: Option<OffsetDateTime>,
) -> ApiResult<NodeRow> {
    let old_hash: [u8; 32] = node
        .content_hash
        .as_deref()
        .and_then(|h| h.try_into().ok())
        .ok_or_else(|| ApiError::Internal("Datei ohne Inhalt in der Datenbank".into()))?;
    let changed_outside = || {
        ApiError::Conflict(format!(
            "„{}“ wurde außerhalb von xlrx geändert. Die Ansicht ist jetzt aktuell; bitte noch einmal versuchen.",
            node.name
        ))
    };

    // Keep the old content as a version: clone it, and make sure it is what the database knows.
    let version_rel = store::version_rel(&old_hash);
    let version = p.state_dir.join(&version_rel);
    let version_tmp = p.state_dir.join(store::VERSIONS).join(store::temp_name());
    let (src, vt) = (path.to_path_buf(), version_tmp.clone());
    let known = node.fingerprint();
    let kept = blocking(
        move || -> std::io::Result<Option<(xlrx_chunk::FileId, Fingerprint)>> {
            let before = stat(&src)?;
            store::clone_file(&src, &vt)?;
            let after = stat(&src)?;
            if before != after {
                return Ok(None);
            }
            if known != Some(before.1) {
                // Not confirmed by a trusted fingerprint: compare the content itself.
                let same = match digest_file(&mut Chunker::new(), &vt)? {
                    FileDigest::Stable { digest, .. } => digest.content.hash.0 == old_hash,
                    FileDigest::ChangedDuringRead => false,
                };
                if !same {
                    return Ok(None);
                }
            }
            Ok(Some(before))
        },
    )
    .await?
    .map_err(io_err)?;
    let Some(old_disk) = kept else {
        let _ = std::fs::remove_file(&version_tmp);
        if let Err(e) = scan::scan_root(&st.db, &p.data_dir, &p.root).await {
            tracing::warn!(root = p.root.id, error = ?e, "Abgleich fehlgeschlagen");
        }
        return Err(changed_outside());
    };
    let (vt, v) = (version_tmp.clone(), version.clone());
    blocking(move || {
        if let Some(d) = v.parent() {
            std::fs::create_dir_all(d)?;
        }
        match store::rename_noreplace(&vt, &v) {
            // The same content is already kept (content-addressed).
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => std::fs::remove_file(&vt),
            r => r,
        }
    })
    .await?
    .map_err(io_err)?;

    let old = OldVersion {
        node_id: node.id,
        rev: node.rev,
        hash: hex(&old_hash),
        size: node.size.unwrap_or_default(),
        mtime: node.mtime,
        store_path: version_rel.clone(),
    };
    let dir = path.parent().unwrap_or(Path::new("/")).to_path_buf();
    let temp = dir.join(store::temp_name());
    let op = record(
        st,
        &upload_intent(p, user_id, staged, &temp, path, Some(old.clone()))?,
    )
    .await?;
    let (s, t, g) = (staged.path.clone(), temp.clone(), path.to_path_buf());
    let swapped = blocking(move || -> std::io::Result<bool> {
        prepare_temp(&s, &t, mtime, Some(&g))?;
        let done = swap_in(&t, &g, old_disk)?;
        store::fsync_dir(&dir)?;
        Ok(done)
    })
    .await?;
    match swapped {
        Ok(true) => {}
        Ok(false) => {
            forget(st, op).await?;
            if let Err(e) = scan::scan_root(&st.db, &p.data_dir, &p.root).await {
                tracing::warn!(root = p.root.id, error = ?e, "Abgleich fehlgeschlagen");
            }
            return Err(changed_outside());
        }
        Err(e) => {
            forget(st, op).await?;
            return Err(if disk_full(&e) {
                ApiError::Conflict("Kein Platz mehr auf dem NAS.".into())
            } else {
                io_err(e)
            });
        }
    }
    let meta = std::fs::symlink_metadata(path).map_err(io_err)?;
    let (fid, _) = fingerprint_of(&meta);
    let mut tx = db::begin_write(&st.db).await?;
    insert_version(&mut tx, &old, Some(user_id)).await?;
    db::set_content(
        &mut tx,
        &node,
        staged.hash,
        staged.size,
        meta.modified().ok().map(OffsetDateTime::from),
        Some(OnDisk { id: fid, fp: None }),
        Source::api(user_id),
    )
    .await?;
    sqlx::query("DELETE FROM pending_ops WHERE id = $1")
        .bind(op)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    reload(st, node.id).await
}

/// Puts the prepared `temp` in place of `target`, but only if `target` is still exactly the file
/// `expected` (identity and fingerprint) whose content was kept as a version. Returns false (and
/// leaves `target` as it is) if it changed meanwhile.
fn swap_in(
    temp: &Path,
    target: &Path,
    expected: (xlrx_chunk::FileId, Fingerprint),
) -> std::io::Result<bool> {
    match store::exchange(temp, target) {
        Ok(()) => {
            // `temp` now holds the file that was in place: exactly the kept version? (The swap
            // itself changed its ctime, so identity, size and mtime are compared.)
            let (id, fp) = stat(temp)?;
            if id != expected.0 || fp.size != expected.1.size || fp.mtime_ns != expected.1.mtime_ns
            {
                store::exchange(temp, target)?;
                std::fs::remove_file(temp)?;
                return Ok(false);
            }
            std::fs::remove_file(temp)?;
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => {
            // No atomic swap on this file system: check right before replacing.
            if stat(target)? != expected {
                std::fs::remove_file(temp)?;
                return Ok(false);
            }
            std::fs::rename(temp, target)?;
            Ok(true)
        }
        Err(e) => {
            let _ = std::fs::remove_file(temp);
            Err(e)
        }
    }
}

async fn insert_version(tx: &mut db::Tx, old: &OldVersion, by: Option<i64>) -> ApiResult<()> {
    let hash = unhex(&old.hash).ok_or_else(|| ApiError::Internal("Hash".into()))?;
    sqlx::query(
        "INSERT INTO versions (node_id, rev, content_hash, size, mtime, store_path, created_by)
         SELECT $1, $2, $3, $4, $5, $6, $7
         WHERE NOT EXISTS (SELECT 1 FROM versions WHERE node_id = $1 AND rev = $2)",
    )
    .bind(old.node_id)
    .bind(old.rev)
    .bind(hash.to_vec())
    .bind(old.size)
    .bind(old.mtime)
    .bind(&old.store_path)
    .bind(by)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// After a crash during an upload: the temp file is removed only if its content is known (the new
/// content, or the old one kept as a version), otherwise kept under a new name for the scan. If
/// the target already holds the new content, the replaced version is recorded; the scan that
/// follows updates the node.
#[allow(clippy::too_many_arguments)]
pub(super) async fn recover_upload(
    st: &AppState,
    p: &Place,
    op: i64,
    staging: &Path,
    temp: &Path,
    target: &Path,
    hash: &str,
    old: Option<OldVersion>,
) -> ApiResult<()> {
    let new_hash = unhex(hash);
    let old_hash = old.as_ref().and_then(|o| unhex(&o.hash));
    let version_kept = old
        .as_ref()
        .is_some_and(|o| p.state_dir.join(&o.store_path).is_file());
    let (temp_abs, target_abs) = (p.data_dir.join(temp), p.data_dir.join(target));
    let digest = |path: &Path| -> Option<[u8; 32]> {
        match digest_file(&mut Chunker::new(), path).ok()? {
            FileDigest::Stable { digest, .. } => Some(digest.content.hash.0),
            FileDigest::ChangedDuringRead => None,
        }
    };
    let is_temp = temp_abs.file_name().is_some_and(|n| {
        n.to_string_lossy()
            .starts_with(super::fs::SERVER_TEMP_PREFIX)
    });
    let target_hash = {
        let (t, tt) = (temp_abs.clone(), target_abs.clone());
        blocking(move || -> std::io::Result<Option<[u8; 32]>> {
            if is_temp && std::fs::symlink_metadata(&t).is_ok() {
                let h = digest(&t);
                let known = h.is_some() && (h == new_hash || (h == old_hash && version_kept));
                if known {
                    std::fs::remove_file(&t)?;
                } else {
                    // Unknown content: never removed. The scan picks it up as "Name (gerettet).ext".
                    let name = tt.file_name().unwrap_or_default().to_string_lossy().into_owned();
                    let base = xlrx_proto::Name::new(&name)
                        .unwrap_or_else(|_| xlrx_proto::Name::new("Datei").expect("valid"));
                    let mut done = false;
                    for i in 1..100 {
                        let suffix = if i == 1 { "gerettet".to_owned() } else { format!("gerettet {i}") };
                        let rescued = t.with_file_name(base.with_suffix(&suffix).as_str());
                        match store::rename_noreplace(&t, &rescued) {
                            Ok(()) => {
                                done = true;
                                break;
                            }
                            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                            Err(e) => return Err(e),
                        }
                    }
                    if !done {
                        tracing::warn!(path = %t.display(), "Unbekannte temporäre Datei bleibt liegen");
                    }
                }
            }
            Ok(if std::fs::symlink_metadata(&tt).is_ok() { digest(&tt) } else { None })
        })
        .await?
        .map_err(io_err)?
    };
    if let Some(old) =
        old.filter(|_| target_hash.is_some() && target_hash == new_hash && version_kept)
    {
        let mut tx = db::begin_write(&st.db).await?;
        insert_version(&mut tx, &old, None).await?;
        tx.commit().await?;
    }
    let _ = std::fs::remove_file(p.state_dir.join(staging));
    forget(st, op).await
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct VersionInfo {
    pub id: i64,
    pub rev: i64,
    pub size: i64,
    #[serde(with = "time::serde::rfc3339::option")]
    pub mtime: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    /// Display name of whoever replaced it (none for changes from outside xlrx).
    pub created_by: Option<String>,
}

async fn readable_node(st: &AppState, user_id: i64, id: i64) -> ApiResult<NodeRow> {
    let node = db::node_by_id(&st.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let root = db::root_by_id(&st.db, node.root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !roots::can_read(&root, user_id) {
        return Err(ApiError::NotFound);
    }
    live(node)
}

/// Earlier contents of a file, newest first.
pub async fn versions(st: &AppState, user_id: i64, node_id: i64) -> ApiResult<Vec<VersionInfo>> {
    readable_node(st, user_id, node_id).await?;
    Ok(sqlx::query_as(
        "SELECT v.id, v.rev, v.size, v.mtime, v.created_at, u.display_name AS created_by
         FROM versions v LEFT JOIN users u ON u.id = v.created_by
         WHERE v.node_id = $1 ORDER BY v.rev DESC",
    )
    .bind(node_id)
    .fetch_all(&st.db)
    .await?)
}

/// A version the person may read: (file in the store, node, version row).
pub async fn version_file(
    st: &AppState,
    user_id: i64,
    version_id: i64,
) -> ApiResult<(PathBuf, NodeRow, VersionRow)> {
    let v: VersionRow = sqlx::query_as(
        "SELECT id, node_id, rev, content_hash, size, mtime, store_path FROM versions WHERE id = $1",
    )
    .bind(version_id)
    .fetch_optional(&st.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let node = readable_node(st, user_id, v.node_id).await?;
    Ok((state_dir(st)?.join(&v.store_path), node, v))
}

#[derive(Debug, sqlx::FromRow)]
pub struct VersionRow {
    pub id: i64,
    pub node_id: i64,
    pub rev: i64,
    pub content_hash: Vec<u8>,
    pub size: i64,
    pub mtime: Option<OffsetDateTime>,
    pub store_path: String,
}

/// Makes an earlier version the current content again (the current one becomes a version).
pub async fn restore_version(st: &AppState, user_id: i64, version_id: i64) -> ApiResult<NodeRow> {
    let (file, node, v) = version_file(st, user_id, version_id).await?;
    let dir = state_dir(st)?;
    let staging = dir
        .join(store::STAGING)
        .join(uuid::Uuid::new_v4().simple().to_string());
    let s = staging.clone();
    let digest = blocking(move || {
        store::init(&dir)?;
        store::clone_file(&file, &s)?;
        digest_file(&mut Chunker::new(), &s)
    })
    .await?;
    let mut staged = Staged {
        path: staging,
        hash: [0; 32],
        size: 0,
    };
    match digest.map_err(io_err)? {
        FileDigest::Stable { digest, .. } if digest.content.hash.0[..] == v.content_hash[..] => {
            staged.hash = digest.content.hash.0;
            staged.size = digest.content.size;
        }
        _ => {
            return Err(ApiError::Internal(format!(
                "Version {} ist beschädigt",
                v.id
            )));
        }
    }
    let (node, _) = write(
        st,
        user_id,
        Target::Replace {
            node_id: node.id,
            base_rev: node.rev,
        },
        staged,
        v.mtime,
    )
    .await?;
    Ok(node)
}

/// Removes version files no version refers to (crash leftovers), when older than a day.
pub async fn collect_versions(st: &AppState) -> ApiResult<()> {
    let Ok(dir) = state_dir(st) else {
        return Ok(());
    };
    let referenced: std::collections::HashSet<String> =
        sqlx::query_scalar::<_, String>("SELECT DISTINCT store_path FROM versions")
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    blocking(move || {
        let root = dir.join(store::VERSIONS);
        let now = std::time::SystemTime::now();
        let mut stack = vec![root];
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let rel = path
                    .strip_prefix(&dir)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .into_owned();
                let old = e
                    .metadata()
                    .and_then(|m| m.modified())
                    .is_ok_and(|t| now.duration_since(t).is_ok_and(|a| a.as_secs() > 86_400));
                if old && !referenced.contains(&rel) {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        Ok(())
    })
    .await?
    .map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_in_only_over_the_expected_file() {
        let d = tempfile::tempdir().unwrap();
        let (temp, target) = (d.path().join(".xlrx-srv-1"), d.path().join("Bericht.txt"));
        std::fs::write(&target, "alt").unwrap();
        let expected = stat(&target).unwrap();

        // Changed after it was kept as a version: stays, the new content is dropped.
        std::fs::write(&target, "per SMB").unwrap();
        std::fs::write(&temp, "neu").unwrap();
        assert!(!swap_in(&temp, &target, expected).unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "per SMB");
        assert!(!temp.exists());

        // Unchanged: swapped in, the old file is gone.
        let expected = stat(&target).unwrap();
        std::fs::write(&temp, "neu").unwrap();
        assert!(swap_in(&temp, &target, expected).unwrap());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "neu");
        assert!(!temp.exists());
    }

    #[test]
    fn hex_round_trip() {
        let h: [u8; 32] = std::array::from_fn(|i| (i * 7) as u8);
        assert_eq!(unhex(&hex(&h)), Some(h));
        assert_eq!(unhex("zz"), None);
    }
}

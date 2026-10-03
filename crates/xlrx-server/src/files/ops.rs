//! Changes through xlrx (PLAN 4.3): create folders, rename and move, trash, restore, purge.
//!
//! Every change holds the lock of its root, so it never overlaps a scan or another change of the
//! same tree. The disk is changed first, then node and journal are committed. A crash in between
//! leaves at worst a change that the next scan picks up.
//!
//! Moving into the trash and back is not one atomic step on the NAS: the trash lives in
//! `xlrx-state`, another Btrfs subvolume, so the tree is cloned and the source removed
//! afterwards. Such changes are announced in `pending_ops` first; [`recover`] completes or rolls
//! them back after a crash. The rule: never remove anything that might be the only copy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use xlrx_chunk::fingerprint_of;
use xlrx_proto::Name;
use xlrx_sync::Reject;

use super::access::{self, Role};
use super::db::{self, NODE_COLS, NodeRow, OnDisk, RootRow, Source};
use super::fs::{ignored, walk};
use super::store::{self, Moved};
use super::{roots, scan};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Items stay this long in the trash.
pub const TRASH_DAYS: i64 = 30;

/// Where a change happens.
pub(super) struct Place {
    pub(super) root: RootRow,
    pub(super) data_dir: PathBuf,
    /// Directory of the root.
    pub(super) dir: PathBuf,
    pub(super) state_dir: PathBuf,
    pub(super) force_copy: bool,
}

pub(super) fn place(st: &AppState, root: RootRow) -> ApiResult<Place> {
    let missing = || ApiError::Internal("Kein Datenverzeichnis konfiguriert".into());
    let data_dir = st.cfg.data_dir.clone().ok_or_else(missing)?;
    let state_dir = st.cfg.state_dir.clone().ok_or_else(missing)?;
    Ok(Place {
        dir: roots::dir(&data_dir, &root),
        root,
        data_dir,
        state_dir,
        force_copy: st.cfg.force_copy,
    })
}

pub(super) fn io_err(e: std::io::Error) -> ApiError {
    ApiError::Internal(e.to_string())
}

/// Runs blocking file system work off the async threads.
pub(super) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> ApiResult<std::io::Result<T>> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))
}

/// Checks and normalizes (NFC) a name chosen through the API.
pub fn valid_name(raw: &str) -> ApiResult<String> {
    let trimmed = raw.trim();
    if trimmed.chars().any(char::is_control) {
        return Err(ApiError::bad("Der Name enthält Steuerzeichen."));
    }
    let name = Name::new(trimmed).map_err(|e| ApiError::bad(e.to_string()))?;
    if ignored(name.as_str()) {
        return Err(ApiError::bad(
            "Dieser Name ist für Systemdateien reserviert.",
        ));
    }
    Ok(name.as_str().to_owned())
}

/// A node in a folder the person may change (create, rename, move, delete there), with its root.
pub(super) async fn changeable(
    st: &AppState,
    user_id: i64,
    id: i64,
) -> ApiResult<(NodeRow, RootRow)> {
    let a = access::require_in_parent(&st.db, user_id, id).await?;
    Ok((a.node, a.root))
}

/// A node the person may change (a folder to add to, a file to write), with its root.
pub(super) async fn writable(
    st: &AppState,
    user_id: i64,
    id: i64,
) -> ApiResult<(NodeRow, RootRow)> {
    let a = access::require(&st.db, user_id, id, Role::Editor).await?;
    Ok((a.node, a.root))
}

/// The root, if the person may manage its trash (owner, or member who may edit).
async fn trash_keeper(st: &AppState, user_id: i64, root: &RootRow) -> ApiResult<()> {
    match roots::role(st, root, user_id).await? {
        Some(r) if r >= Role::Editor => Ok(()),
        Some(_) => Err(ApiError::forbidden("Du darfst hier nur ansehen.")),
        None => Err(ApiError::NotFound),
    }
}

pub(super) fn live(node: NodeRow) -> ApiResult<NodeRow> {
    if node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    Ok(node)
}

pub(super) fn check_seq(node: &NodeRow, if_seq: Option<i64>) -> ApiResult<()> {
    match if_seq {
        Some(s) if s != node.seq => Err(ApiError::Conflict(format!(
            "„{}“ wurde inzwischen geändert. Bitte die Ansicht neu laden.",
            node.name
        ))),
        _ => Ok(()),
    }
}

/// Path of a live node on disk, after checking that the disk still holds this node there. If not
/// (changed outside xlrx and not scanned yet), the root is scanned and the change refused.
pub(super) async fn located(st: &AppState, p: &Place, node: &NodeRow) -> ApiResult<PathBuf> {
    let rel = db::rel_path(&st.db, node.id).await?;
    let path = if rel.as_os_str().is_empty() {
        p.dir.clone()
    } else {
        p.dir.join(rel)
    };
    let same = std::fs::symlink_metadata(&path)
        .is_ok_and(|m| m.is_dir() == node.is_dir() && node.file_id() == Some(fingerprint_of(&m).0));
    if !same {
        if let Err(e) = scan::scan_root(&st.db, &p.data_dir, &p.root).await {
            tracing::warn!(root = p.root.id, error = ?e, "Abgleich nach Abweichung fehlgeschlagen");
        }
        return Err(ApiError::Conflict(format!(
            "„{}“ wurde außerhalb von xlrx verändert. Die Ansicht ist jetzt aktuell – bitte noch einmal versuchen.",
            node.name
        )));
    }
    Ok(path)
}

/// Refuses a name that is taken in the folder, in the database or on disk, ignoring case
/// (Macs and SMB would see a collision). `except`: the node being renamed itself.
pub(super) async fn ensure_free(
    st: &AppState,
    parent: &NodeRow,
    dir: &Path,
    name: &str,
    except: Option<&NodeRow>,
) -> ApiResult<()> {
    let taken = || {
        ApiError::Rejected(
            Reject::NameTaken,
            format!("In diesem Ordner gibt es schon „{name}“."),
        )
    };
    let in_db: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nodes WHERE parent_id = $1 AND name_folded = $2
                          AND deleted_at IS NULL AND id <> $3)",
    )
    .bind(parent.id)
    .bind(db::fold(name))
    .bind(except.map_or(0, |n| n.id))
    .fetch_one(&st.db)
    .await?;
    if in_db {
        return Err(taken());
    }
    // Case-only rename of the node itself: on disk, only the node itself matches.
    let self_rename = except
        .is_some_and(|n| n.parent_id == Some(parent.id) && db::fold(&n.name) == db::fold(name));
    if !self_rename && store::name_taken(dir, name).map_err(io_err)? {
        return Err(taken());
    }
    Ok(())
}

/// Is the name taken in the folder according to the database (a quick check before receiving an
/// upload; the write itself checks again, also on disk)?
pub async fn name_in_use(st: &AppState, parent_id: i64, name: &str) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM nodes WHERE parent_id = $1 AND name_folded = $2 AND deleted_at IS NULL)",
    )
    .bind(parent_id)
    .bind(db::fold(name))
    .fetch_one(&st.db)
    .await?)
}

pub(super) async fn reload(st: &AppState, id: i64) -> ApiResult<NodeRow> {
    db::node_by_id(&st.db, id).await?.ok_or(ApiError::NotFound)
}

/// Creates a folder.
pub async fn mkdir(
    st: &AppState,
    user_id: i64,
    parent_id: i64,
    raw_name: &str,
) -> ApiResult<NodeRow> {
    let name = valid_name(raw_name)?;
    let (_, root) = writable(st, user_id, parent_id).await?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let (parent, root) = writable(st, user_id, parent_id).await?;
    let parent = live(parent)?;
    if !parent.is_dir() {
        return Err(ApiError::Rejected(
            Reject::ParentGone,
            "Kein Ordner.".into(),
        ));
    }
    let p = place(st, root)?;
    let dir = located(st, &p, &parent).await?;
    ensure_free(st, &parent, &dir, &name, None).await?;
    let path = dir.join(&name);
    let meta = blocking(move || {
        std::fs::create_dir(&path)?;
        store::fsync_dir(&dir)?;
        std::fs::symlink_metadata(&path)
    })
    .await?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => ApiError::Rejected(
            Reject::NameTaken,
            format!("In diesem Ordner gibt es schon „{name}“."),
        ),
        _ => io_err(e),
    })?;
    let (id, fp) = fingerprint_of(&meta);
    let mut tx = db::begin_write(&st.db).await?;
    let (node_id, _) = db::insert_node(
        &mut tx,
        &db::NewNode {
            root_id: p.root.id,
            parent_id: Some(parent.id),
            name: &name,
            is_dir: true,
            content: None,
            mtime: meta.modified().ok().map(OffsetDateTime::from),
            disk: Some(OnDisk { id, fp: Some(fp) }),
        },
        Source::api(user_id),
    )
    .await?;
    tx.commit().await?;
    reload(st, node_id).await
}

/// Rename and/or move within the same root.
#[derive(Debug, Default, Deserialize)]
pub struct Change {
    pub name: Option<String>,
    pub parent_id: Option<i64>,
    /// Only if the node is still at this state (`seq` as last seen).
    pub if_seq: Option<i64>,
    /// Only if the node is still in this folder under this name (sync clients).
    #[serde(skip)]
    pub from: Option<(i64, String)>,
}

/// Is the node still where the caller saw it? (Otherwise someone else moved it: their move wins.)
fn check_at(node: &NodeRow, at: Option<&(i64, String)>) -> ApiResult<()> {
    match at {
        Some((parent, name)) if node.parent_id != Some(*parent) || node.name != *name => {
            Err(ApiError::Rejected(
                Reject::Moved,
                format!(
                    "„{}“ wurde inzwischen verschoben oder umbenannt.",
                    node.name
                ),
            ))
        }
        _ => Ok(()),
    }
}

pub async fn update(st: &AppState, user_id: i64, id: i64, ch: Change) -> ApiResult<NodeRow> {
    let (_, root) = changeable(st, user_id, id).await?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let (node, root) = changeable(st, user_id, id).await?;
    let node = live(node)?;
    let Some(old_parent) = node.parent_id else {
        return Err(ApiError::bad(
            "Die Ablage selbst kann nicht umbenannt oder verschoben werden.",
        ));
    };
    check_seq(&node, ch.if_seq)?;
    check_at(&node, ch.from.as_ref())?;
    let name = match &ch.name {
        Some(n) => valid_name(n)?,
        None => node.name.clone(),
    };
    let parent_id = ch.parent_id.unwrap_or(old_parent);
    if parent_id == old_parent && name == node.name {
        return Ok(node);
    }
    let target = db::node_by_id(&st.db, parent_id)
        .await?
        .filter(|t| t.deleted_at.is_none())
        .ok_or_else(|| {
            ApiError::Rejected(
                Reject::ParentGone,
                "Den Zielordner gibt es nicht mehr.".into(),
            )
        })?;
    if target.root_id != node.root_id {
        return Err(ApiError::bad(
            "Verschieben in eine andere Ablage ist noch nicht möglich.",
        ));
    }
    if target.id != old_parent {
        // The target must be a folder the person may add to, too.
        writable(st, user_id, target.id).await?;
    }
    if !target.is_dir() {
        return Err(ApiError::Rejected(
            Reject::ParentGone,
            "Das Ziel ist kein Ordner.".into(),
        ));
    }
    if db::ancestors(&st.db, target.id)
        .await?
        .iter()
        .any(|a| a.id == node.id)
    {
        return Err(ApiError::Rejected(
            Reject::WouldCycle,
            "Ein Ordner kann nicht in sich selbst verschoben werden.".into(),
        ));
    }
    let p = place(st, root)?;
    let src = located(st, &p, &node).await?;
    let dst_dir = located(st, &p, &target).await?;
    ensure_free(st, &target, &dst_dir, &name, Some(&node)).await?;
    let dst = dst_dir.join(&name);
    let meta = blocking(move || {
        store::rename_noreplace(&src, &dst)?;
        if let Some(d) = src.parent() {
            store::fsync_dir(d)?;
        }
        store::fsync_dir(&dst_dir)?;
        std::fs::symlink_metadata(&dst)
    })
    .await?
    .map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => ApiError::Rejected(
            Reject::NameTaken,
            format!("Im Ziel gibt es schon „{name}“."),
        ),
        _ => io_err(e),
    })?;
    let mut tx = db::begin_write(&st.db).await?;
    db::set_location(&mut tx, &node, target.id, &name, Source::api(user_id)).await?;
    // A rename changes ctime. Keep the stored fingerprint current so the next scan does not read
    // the file again, unless size or mtime differ (then the content may have changed meanwhile).
    let (fid, fp) = fingerprint_of(&meta);
    let unchanged = node
        .fingerprint()
        .is_some_and(|old| old.size == fp.size && old.mtime_ns == fp.mtime_ns);
    if node.is_dir() || unchanged {
        db::set_disk(
            &mut tx,
            node.id,
            OnDisk {
                id: fid,
                fp: Some(fp),
            },
            None,
        )
        .await?;
    }
    tx.commit().await?;
    reload(st, node.id).await
}

/// A step that is not atomic on disk, recorded in `pending_ops` (see the module docs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum Intent {
    Trash {
        root_id: i64,
        node_id: i64,
        actor: i64,
        /// Relative to the data directory.
        src: PathBuf,
        /// Relative to the state directory: `store/trash/<container>/<name>`.
        dst: PathBuf,
        /// The copy is complete and verified; only removing the source is left.
        copied: bool,
    },
    Restore {
        root_id: i64,
        node_id: i64,
        actor: i64,
        /// Relative to the state directory.
        src: PathBuf,
        /// Relative to the data directory.
        dst: PathBuf,
        /// Temporary name next to `dst` while copying (relative to the data directory).
        stage: PathBuf,
        parent_id: i64,
        name: String,
        copied: bool,
    },
    /// A file written through xlrx (upload or restored version), see [`super::content`].
    Upload {
        root_id: i64,
        actor: i64,
        /// Relative to the state directory.
        staging: PathBuf,
        /// Server temp name next to the target (relative to the data directory).
        temp: PathBuf,
        /// Relative to the data directory.
        target: PathBuf,
        /// Content written (hex).
        hash: String,
        /// The replaced content, kept as a version.
        old: Option<super::content::OldVersion>,
    },
}

impl Intent {
    fn kind(&self) -> &'static str {
        match self {
            Intent::Trash { .. } => "trash",
            Intent::Restore { .. } => "restore",
            Intent::Upload { .. } => "upload",
        }
    }

    fn root_id(&self) -> i64 {
        match self {
            Intent::Trash { root_id, .. }
            | Intent::Restore { root_id, .. }
            | Intent::Upload { root_id, .. } => *root_id,
        }
    }
}

pub(super) async fn record(st: &AppState, intent: &Intent) -> ApiResult<i64> {
    let payload = serde_json::to_value(intent).map_err(|e| ApiError::Internal(e.to_string()))?;
    Ok(
        sqlx::query_scalar("INSERT INTO pending_ops (kind, payload) VALUES ($1, $2) RETURNING id")
            .bind(intent.kind())
            .bind(payload)
            .fetch_one(&st.db)
            .await?,
    )
}

pub(super) async fn mark_copied(st: &AppState, op: i64) -> ApiResult<()> {
    sqlx::query(
        "UPDATE pending_ops SET payload = jsonb_set(payload, '{copied}', 'true') WHERE id = $1",
    )
    .bind(op)
    .execute(&st.db)
    .await?;
    Ok(())
}

pub(super) async fn forget(st: &AppState, op: i64) -> ApiResult<()> {
    sqlx::query("DELETE FROM pending_ops WHERE id = $1")
        .bind(op)
        .execute(&st.db)
        .await?;
    Ok(())
}

/// Preconditions of a deletion by a sync client.
#[derive(Debug, Default, Clone)]
pub struct DeleteIf {
    /// Still in this folder under this name.
    pub at: Option<(i64, String)>,
    /// Files: still this content revision.
    pub rev: Option<i64>,
    /// Folders: only if empty.
    pub empty: bool,
}

/// Moves a node with everything below it into the trash.
pub async fn trash(st: &AppState, user_id: i64, id: i64, if_seq: Option<i64>) -> ApiResult<()> {
    trash_if(st, user_id, id, if_seq, &DeleteIf::default()).await
}

/// [`trash`] with the preconditions of a sync client.
pub async fn trash_if(
    st: &AppState,
    user_id: i64,
    id: i64,
    if_seq: Option<i64>,
    pre: &DeleteIf,
) -> ApiResult<()> {
    let (_, root) = changeable(st, user_id, id).await?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let (node, root) = changeable(st, user_id, id).await?;
    let node = live(node)?;
    if node.parent_id.is_none() {
        return Err(ApiError::bad(
            "Die Ablage selbst kann nicht gelöscht werden.",
        ));
    }
    check_seq(&node, if_seq)?;
    if pre.rev.is_some_and(|r| r != node.rev) {
        return Err(ApiError::Rejected(
            Reject::RevMismatch,
            format!("„{}“ wurde inzwischen geändert.", node.name),
        ));
    }
    check_at(&node, pre.at.as_ref())?;
    if pre.empty && !db::children_of(&st.db, &[node.id]).await?.is_empty() {
        return Err(ApiError::Rejected(
            Reject::NotEmpty,
            format!("„{}“ ist nicht leer.", node.name),
        ));
    }
    let p = place(st, root)?;
    let src = located(st, &p, &node).await?;
    let container = format!(
        "{}-{}",
        node.id,
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let dst_rel = Path::new(store::TRASH).join(container).join(&node.name);
    let dst = p.state_dir.join(&dst_rel);
    let intent = Intent::Trash {
        root_id: p.root.id,
        node_id: node.id,
        actor: user_id,
        src: src
            .strip_prefix(&p.data_dir)
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .to_path_buf(),
        dst: dst_rel.clone(),
        copied: false,
    };
    let state_dir = p.state_dir.clone();
    let parent = dst.parent().map(Path::to_path_buf).unwrap_or_default();
    blocking(move || {
        store::init(&state_dir)?;
        std::fs::create_dir(&parent)
    })
    .await?
    .map_err(io_err)?;
    let op = record(st, &intent).await?;
    let (s, d, force) = (src.clone(), dst.clone(), p.force_copy);
    match blocking(move || store::move_or_copy(&s, &d, None, force)).await? {
        Ok(Moved::Renamed) => {}
        Ok(Moved::Copied) => {
            if let Err(e) = mark_copied(st, op).await {
                // The source is untouched: drop the copy and give up.
                let _ = blocking(move || store::remove_tree(&dst)).await;
                let _ = forget(st, op).await;
                return Err(e);
            }
            if let Err(e) = blocking(move || store::finish_copy(&src)).await? {
                // The trash holds the complete copy; leftovers reappear with the next scan.
                tracing::warn!(node = node.id, error = %e, "Quelle nach dem Kopieren nicht ganz entfernt");
            }
        }
        Err(e) => {
            forget(st, op).await?;
            return Err(match e.kind() {
                std::io::ErrorKind::Interrupted => ApiError::Conflict(format!(
                    "„{}“ wurde während des Löschens verändert. Bitte noch einmal versuchen.",
                    node.name
                )),
                _ => io_err(e),
            });
        }
    }
    commit_trash(st, p.root.id, node.id, user_id, &dst_rel, op).await
}

/// Marks the node and everything below it as deleted (in the trash) and drops the intent.
async fn commit_trash(
    st: &AppState,
    root_id: i64,
    top: i64,
    actor: i64,
    trash: &Path,
    op: i64,
) -> ApiResult<()> {
    let mut tx = db::begin_write(&st.db).await?;
    sqlx::query(
        "WITH RECURSIVE sub AS (
           SELECT id FROM nodes WHERE id = $1 AND deleted_at IS NULL
           UNION ALL
           SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id WHERE n.deleted_at IS NULL),
         j AS (
           INSERT INTO journal (seq, root_id, node_id, op, source, actor_user_id)
           SELECT nextval('journal_seq'), $2, id, 'delete', 'api', $3 FROM sub
           RETURNING node_id, seq)
         UPDATE nodes n SET deleted_at = now(), deleted_by = $3, deleted_with = $1,
                trash_path = CASE WHEN n.id = $1 THEN $4 END, seq = j.seq, updated_at = now()
         FROM j WHERE n.id = j.node_id",
    )
    .bind(top)
    .bind(root_id)
    .bind(actor)
    .bind(trash.to_string_lossy().as_ref())
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM pending_ops WHERE id = $1")
        .bind(op)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// A trashed item (the top of a deletion).
#[derive(Debug, Clone, Serialize)]
pub struct TrashItem {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub size: Option<i64>,
    #[serde(with = "time::serde::rfc3339")]
    pub deleted_at: OffsetDateTime,
    /// Folder it was deleted from, e.g. "Meine Ablage/Projekte".
    pub from: String,
}

pub async fn trash_list(st: &AppState, user_id: i64, root_id: i64) -> ApiResult<Vec<TrashItem>> {
    let root = db::root_by_id(&st.db, root_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    trash_keeper(st, user_id, &root).await?;
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE root_id = $1 AND trash_path IS NOT NULL
         ORDER BY deleted_at DESC LIMIT 1000"
    )))
    .bind(root_id)
    .fetch_all(&st.db)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for n in rows {
        let mut from = vec![root.name.clone()];
        if let Some(parent) = n.parent_id {
            from.extend(
                db::ancestors(&st.db, parent)
                    .await?
                    .into_iter()
                    .skip(1)
                    .map(|a| a.name),
            );
        }
        out.push(TrashItem {
            id: n.id,
            name: n.name,
            kind: n.kind,
            size: n.size,
            deleted_at: n.deleted_at.unwrap_or_else(OffsetDateTime::now_utc),
            from: from.join("/"),
        });
    }
    Ok(out)
}

async fn trash_path_of(st: &AppState, id: i64) -> ApiResult<Option<PathBuf>> {
    let p: Option<Option<String>> =
        sqlx::query_scalar("SELECT trash_path FROM nodes WHERE id = $1")
            .bind(id)
            .fetch_optional(&st.db)
            .await?;
    Ok(p.flatten().map(PathBuf::from))
}

/// Container directory of a trash entry (`store/trash/<container>`), refusing anything else.
fn trash_container(trash_rel: &Path) -> ApiResult<PathBuf> {
    let parent = trash_rel.parent().unwrap_or(Path::new(""));
    if parent.parent() != Some(Path::new(store::TRASH)) {
        return Err(ApiError::Internal(format!(
            "unerwarteter Papierkorb-Pfad {}",
            trash_rel.display()
        )));
    }
    Ok(parent.to_path_buf())
}

/// Brings an item back from the trash: to its folder if that still exists, otherwise to the top
/// of the root; under a new name if the old one is taken.
pub async fn restore(st: &AppState, user_id: i64, id: i64) -> ApiResult<NodeRow> {
    let (_, root) = writable(st, user_id, id).await?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let (node, root) = writable(st, user_id, id).await?;
    let trash_rel = match (node.deleted_at, trash_path_of(st, id).await?) {
        (Some(_), Some(t)) => t,
        _ => return Err(ApiError::NotFound),
    };
    let p = place(st, root)?;
    let parent = match node.parent_id {
        Some(pid) => db::node_by_id(&st.db, pid)
            .await?
            .filter(|n| n.deleted_at.is_none()),
        None => None,
    };
    let parent = match parent {
        Some(n) => n,
        None => db::root_node(&st.db, p.root.id)
            .await?
            .ok_or(ApiError::NotFound)?,
    };
    // Back into a folder the person may add to (the top of the root needs rights on all of it).
    writable(st, user_id, parent.id).await?;
    let dir = located(st, &p, &parent).await?;
    let base = Name::new(&node.name).map_err(|e| ApiError::Internal(e.to_string()))?;
    let mut name = base.as_str().to_owned();
    for i in 1.. {
        if ensure_free(st, &parent, &dir, &name, None).await.is_ok() {
            break;
        }
        if i > 100 {
            return Err(ApiError::Conflict(
                "Kein freier Name zum Wiederherstellen gefunden.".into(),
            ));
        }
        let suffix = if i == 1 {
            "wiederhergestellt".to_owned()
        } else {
            format!("wiederhergestellt {i}")
        };
        name = base.with_suffix(&suffix).as_str().to_owned();
    }
    let src = p.state_dir.join(&trash_rel);
    if std::fs::symlink_metadata(&src).is_err() {
        return Err(ApiError::Conflict(format!(
            "„{}“ ist nicht mehr im Papierkorb.",
            node.name
        )));
    }
    let dst = dir.join(&name);
    let stage = dir.join(store::temp_name());
    let rel = |path: &Path| -> ApiResult<PathBuf> {
        Ok(path
            .strip_prefix(&p.data_dir)
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .to_path_buf())
    };
    let intent = Intent::Restore {
        root_id: p.root.id,
        node_id: node.id,
        actor: user_id,
        src: trash_rel.clone(),
        dst: rel(&dst)?,
        stage: rel(&stage)?,
        parent_id: parent.id,
        name: name.clone(),
        copied: false,
    };
    let op = record(st, &intent).await?;
    let (s, d, g, force) = (src.clone(), dst.clone(), stage.clone(), p.force_copy);
    match blocking(move || store::move_or_copy(&s, &d, Some(&g), force)).await? {
        Ok(Moved::Renamed) => {}
        Ok(Moved::Copied) => {
            mark_copied(st, op).await?;
            if let Err(e) = blocking(move || store::finish_copy(&src)).await? {
                tracing::warn!(node = node.id, error = %e, "Papierkorb-Kopie nicht ganz entfernt");
            }
        }
        Err(e) => {
            forget(st, op).await?;
            return Err(match e.kind() {
                std::io::ErrorKind::AlreadyExists => ApiError::Rejected(
                    Reject::NameTaken,
                    format!("Im Ordner gibt es schon „{name}“."),
                ),
                _ => io_err(e),
            });
        }
    }
    commit_restore(
        st, &p, node.id, parent.id, &name, user_id, &trash_rel, &dst, op,
    )
    .await?;
    reload(st, node.id).await
}

/// Marks the item and everything deleted with it as live again, at its new place, updates their
/// identity on disk and drops the intent.
#[allow(clippy::too_many_arguments)]
async fn commit_restore(
    st: &AppState,
    p: &Place,
    top: i64,
    parent_id: i64,
    name: &str,
    actor: i64,
    trash_rel: &Path,
    dst: &Path,
    op: i64,
) -> ApiResult<()> {
    let mut tx = db::begin_write(&st.db).await?;
    sqlx::query(
        "WITH j AS (
           INSERT INTO journal (seq, root_id, node_id, op, source, actor_user_id)
           SELECT nextval('journal_seq'), $2, id, 'restore', 'api', $3 FROM nodes
           WHERE deleted_with = $1 AND deleted_at IS NOT NULL
           RETURNING node_id, seq)
         UPDATE nodes n SET deleted_at = NULL, deleted_by = NULL, deleted_with = NULL,
                trash_path = NULL, seq = j.seq, updated_at = now()
         FROM j WHERE n.id = j.node_id",
    )
    .bind(top)
    .bind(p.root.id)
    .bind(actor)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE nodes SET parent_id = $2, name = $3, name_folded = $4 WHERE id = $1")
        .bind(top)
        .bind(parent_id)
        .bind(name)
        .bind(db::fold(name))
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM pending_ops WHERE id = $1")
        .bind(op)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    refresh_disk(st, top, dst.to_path_buf()).await?;
    let container = p.state_dir.join(trash_container(trash_rel)?);
    // Empty by now (a leftover after a partial removal stays for the housekeeping).
    let _ = std::fs::remove_dir(container);
    Ok(())
}

/// Updates the identity on disk of a restored subtree (copies have new inodes). File
/// fingerprints are cleared, so the next scan reads the files once and confirms their content.
async fn refresh_disk(st: &AppState, top: i64, path: PathBuf) -> ApiResult<()> {
    let sub: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE sub AS (
           SELECT id FROM nodes WHERE id = $1
           UNION ALL
           SELECT n.id FROM nodes n JOIN sub ON n.parent_id = sub.id WHERE n.deleted_at IS NULL)
         SELECT {NODE_COLS} FROM nodes WHERE id IN (SELECT id FROM sub)"
    )))
    .bind(top)
    .fetch_all(&st.db)
    .await?;
    // Paths relative to the folder of the restored item ("Name", "Name/Unter", …).
    let paths = db::paths(&sub);
    let by_path: std::collections::HashMap<PathBuf, &NodeRow> = sub
        .iter()
        .filter_map(|n| Some((paths.get(&n.id)?.clone(), n)))
        .collect();
    let seen = blocking(move || {
        let parent = path.parent().unwrap_or(Path::new("/")).to_path_buf();
        let name = PathBuf::from(path.file_name().unwrap_or_default());
        if std::fs::symlink_metadata(&path)?.is_dir() {
            Ok(walk(&parent, &name, true)?.entries)
        } else {
            let m = std::fs::symlink_metadata(&path)?;
            let (id, fp) = fingerprint_of(&m);
            Ok(vec![super::fs::Seen {
                name: name.to_string_lossy().into_owned(),
                rel: name,
                is_dir: false,
                id,
                fp,
                mtime: m.modified().ok().map(OffsetDateTime::from),
            }])
        }
    })
    .await?
    .map_err(io_err)?;
    // The walk starts at the restored item under its (new) name, like `paths` above.
    let mut tx = db::begin_write(&st.db).await?;
    for s in &seen {
        if let Some(n) = by_path.get(&s.rel).filter(|n| n.is_dir() == s.is_dir) {
            let fp = s.is_dir.then_some(s.fp);
            db::set_disk(&mut tx, n.id, OnDisk { id: s.id, fp }, s.mtime).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Removes an item from the trash for good.
pub async fn purge(st: &AppState, user_id: i64, id: i64) -> ApiResult<()> {
    // Only who keeps the trash may destroy for good; people it was shared with never can.
    let (_, root) = writable(st, user_id, id).await?;
    if trash_keeper(st, user_id, &root).await.is_err() {
        return Err(ApiError::forbidden(
            "Endgültig löschen kann nur, wer die ganze Ablage bearbeiten darf.",
        ));
    }
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    let Some(trash_rel) = trash_path_of(st, id).await? else {
        return Err(ApiError::NotFound);
    };
    purge_entry(st, id, &trash_rel).await
}

async fn purge_entry(st: &AppState, id: i64, trash_rel: &Path) -> ApiResult<()> {
    let state_dir = st
        .cfg
        .state_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))?;
    let container = state_dir.join(trash_container(trash_rel)?);
    // Database first: a crash afterwards leaves an unreferenced container for the housekeeping,
    // never a reference to something already gone.
    sqlx::query("UPDATE nodes SET trash_path = NULL WHERE id = $1")
        .bind(id)
        .execute(&st.db)
        .await?;
    blocking(move || store::remove_tree(&container))
        .await?
        .map_err(io_err)
}

/// Empties expired trash entries and removes leftovers: trash containers no node refers to and
/// staging files, each only when older than the trash period or a day respectively.
pub async fn housekeeping(st: &AppState) -> ApiResult<()> {
    let Some(state_dir) = st.cfg.state_dir.clone() else {
        return Ok(());
    };
    let expired: Vec<(i64, i64, String)> = sqlx::query_as(
        "SELECT id, root_id, trash_path FROM nodes
         WHERE trash_path IS NOT NULL AND deleted_at < now() - make_interval(days => $1::int)",
    )
    .bind(TRASH_DAYS as i32)
    .fetch_all(&st.db)
    .await?;
    for (id, root_id, path) in expired {
        let lock = st.root_lock(root_id);
        let _guard = lock.lock().await;
        if let Err(e) = purge_entry(st, id, Path::new(&path)).await {
            tracing::warn!(node = id, error = ?e, "Papierkorb-Eintrag nicht entfernt");
        }
    }
    let referenced: Vec<String> =
        sqlx::query_scalar("SELECT trash_path FROM nodes WHERE trash_path IS NOT NULL")
            .fetch_all(&st.db)
            .await?;
    let pending: Vec<serde_json::Value> = sqlx::query_scalar("SELECT payload FROM pending_ops")
        .fetch_all(&st.db)
        .await?;
    let mut keep: std::collections::HashSet<PathBuf> = referenced
        .iter()
        .filter_map(|p| trash_container(Path::new(p)).ok())
        .collect();
    for v in pending {
        if let Ok(Intent::Trash { dst, .. } | Intent::Restore { src: dst, .. }) =
            serde_json::from_value(v)
        {
            keep.extend(trash_container(&dst).ok());
        }
    }
    blocking(move || {
        let now = std::time::SystemTime::now();
        let older = |p: &Path, secs: u64| {
            std::fs::symlink_metadata(p)
                .and_then(|m| m.modified())
                .is_ok_and(|t| now.duration_since(t).is_ok_and(|d| d.as_secs() > secs))
        };
        if let Ok(rd) = std::fs::read_dir(state_dir.join(store::TRASH)) {
            for e in rd.flatten() {
                let rel = Path::new(store::TRASH).join(e.file_name());
                if !keep.contains(&rel) && older(&e.path(), TRASH_DAYS as u64 * 86_400) {
                    store::remove_tree(&e.path())?;
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir(state_dir.join(store::STAGING)) {
            for e in rd.flatten() {
                if older(&e.path(), 86_400) {
                    store::remove_tree(&e.path())?;
                }
            }
        }
        Ok(())
    })
    .await?
    .map_err(io_err)
}

/// Completes or rolls back changes a crash interrupted (see the module docs). Runs at startup
/// for all roots and before every scan for its root (under the root's lock).
pub async fn recover(st: &AppState, root_id: Option<i64>) -> ApiResult<()> {
    let ops: Vec<(i64, serde_json::Value)> =
        sqlx::query_as("SELECT id, payload FROM pending_ops ORDER BY id")
            .fetch_all(&st.db)
            .await?;
    for (op, payload) in ops {
        let intent: Intent = match serde_json::from_value(payload) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(op, error = %e, "Unbekannter offener Vorgang");
                continue;
            }
        };
        if root_id.is_some_and(|r| r != intent.root_id()) {
            continue;
        }
        if let Err(e) = recover_one(st, op, intent).await {
            tracing::warn!(op, error = ?e, "Offener Vorgang nicht abgeschlossen");
        }
    }
    Ok(())
}

async fn recover_one(st: &AppState, op: i64, intent: Intent) -> ApiResult<()> {
    let root = db::root_by_id(&st.db, intent.root_id()).await?;
    let Some(root) = root else {
        return forget(st, op).await;
    };
    let p = place(st, root)?;
    let exists = |path: &Path| std::fs::symlink_metadata(path).is_ok();
    match intent {
        Intent::Upload {
            staging,
            temp,
            target,
            hash,
            old,
            ..
        } => super::content::recover_upload(st, &p, op, &staging, &temp, &target, &hash, old).await,
        Intent::Trash {
            node_id,
            actor,
            src,
            dst,
            copied,
            ..
        } => {
            let src_abs = p.data_dir.join(&src);
            if copied {
                // The trash holds the verified copy: finish removing the source.
                if let Err(e) = blocking(move || store::finish_copy(&src_abs)).await? {
                    tracing::warn!(node = node_id, error = %e, "Quelle nicht ganz entfernt");
                }
            } else if exists(&src_abs) {
                // Not moved (or the copy was not complete): the original stays. A partial copy
                // stays too and is cleaned up by the housekeeping.
                tracing::info!(node = node_id, "Löschen nach Absturz zurückgenommen");
                return forget(st, op).await;
            }
            let node = db::node_by_id(&st.db, node_id).await?;
            if node.is_some_and(|n| n.deleted_at.is_none()) {
                tracing::info!(node = node_id, "Löschen nach Absturz abgeschlossen");
                commit_trash(st, p.root.id, node_id, actor, &dst, op).await
            } else {
                forget(st, op).await
            }
        }
        Intent::Restore {
            node_id,
            actor,
            src,
            dst,
            stage,
            parent_id,
            name,
            copied,
            ..
        } => {
            let src_abs = p.state_dir.join(&src);
            let stage_abs = p.data_dir.join(&stage);
            // The stage is the server's own temporary name: never anything else.
            if stage_abs.file_name().is_some_and(|n| {
                n.to_string_lossy()
                    .starts_with(super::fs::SERVER_TEMP_PREFIX)
            }) {
                let _ = blocking(move || store::remove_tree(&stage_abs)).await;
            }
            if copied {
                if let Err(e) = blocking(move || store::finish_copy(&src_abs)).await? {
                    tracing::warn!(node = node_id, error = %e, "Papierkorb-Kopie nicht ganz entfernt");
                }
            } else if exists(&src_abs) {
                tracing::info!(
                    node = node_id,
                    "Wiederherstellen nach Absturz zurückgenommen"
                );
                return forget(st, op).await;
            }
            let node = db::node_by_id(&st.db, node_id).await?;
            if node.is_some_and(|n| n.deleted_at.is_some()) {
                tracing::info!(
                    node = node_id,
                    "Wiederherstellen nach Absturz abgeschlossen"
                );
                let dst_abs = p.data_dir.join(&dst);
                commit_restore(st, &p, node_id, parent_id, &name, actor, &src, &dst_abs, op).await
            } else {
                forget(st, op).await
            }
        }
    }
}

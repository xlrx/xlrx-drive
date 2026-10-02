//! Roots: which directories on the NAS xlrx manages, and who may see them.

use std::path::{Component, Path, PathBuf};

use super::db::{self, NewNode, OnDisk, ROOT_COLS, RootRow, Source};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

/// Relative path of a person's home root, e.g. `homes/klaus/Drive`.
pub fn home_rel_path(pattern: &str, username: &str) -> ApiResult<PathBuf> {
    let rel = PathBuf::from(pattern.replace("{user}", username));
    let safe = rel.components().all(|c| matches!(c, Component::Normal(_)));
    if !safe || rel.as_os_str().is_empty() {
        return Err(ApiError::Internal(format!(
            "XLRX_HOME_PATTERN ergibt keinen gültigen Pfad: {}",
            rel.display()
        )));
    }
    Ok(rel)
}

/// The person's "My Drive", created on first use (directory and root node). `None` if the server
/// has no data directory.
pub async fn ensure_home(
    st: &AppState,
    user_id: i64,
    username: &str,
) -> ApiResult<Option<RootRow>> {
    let Some(data_dir) = st.cfg.data_dir.as_deref() else {
        return Ok(None);
    };
    if let Some(r) = home(st, user_id).await? {
        return Ok(Some(r));
    }
    let rel = home_rel_path(&st.cfg.home_pattern, username)?;
    let dir = data_dir.join(&rel);
    std::fs::create_dir_all(&dir)
        .map_err(|e| ApiError::Internal(format!("{}: {e}", dir.display())))?;
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| ApiError::Internal(e.to_string()))?;
    let (id, fp) = xlrx_chunk::fingerprint_of(&meta);
    let rel_str = rel.to_string_lossy().into_owned();
    let name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Drive".into());

    let mut tx = db::begin_write(&st.db).await?;
    // Another request may have created it meanwhile (the journal lock serializes us).
    let existing: Option<RootRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots WHERE kind = 'home' AND owner_user_id = $1"
    )))
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(r) = existing {
        return Ok(Some(r));
    }
    let root: RootRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO roots (kind, name, rel_path, owner_user_id) VALUES ('home', 'Meine Ablage', $1, $2)
         RETURNING {ROOT_COLS}"
    )))
    .bind(&rel_str)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    db::insert_node(
        &mut tx,
        &NewNode {
            root_id: root.id,
            parent_id: None,
            name: &name,
            is_dir: true,
            content: None,
            mtime: None,
            disk: Some(OnDisk { id, fp: Some(fp) }),
        },
        Source::api(user_id),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(root = root.id, path = %rel_str, "Ablage angelegt");
    Ok(Some(root))
}

pub async fn home(st: &AppState, user_id: i64) -> ApiResult<Option<RootRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots WHERE kind = 'home' AND owner_user_id = $1"
    )))
    .bind(user_id)
    .fetch_optional(&st.db)
    .await?)
}

/// May this person see the root? (Home: only its owner. Spaces follow in M3.)
pub fn can_read(root: &RootRow, user_id: i64) -> bool {
    root.owner_user_id == Some(user_id)
}

/// May this person change the root's content? (Home: only its owner.)
pub fn can_write(root: &RootRow, user_id: i64) -> bool {
    root.owner_user_id == Some(user_id)
}

/// Directory of a root on disk.
pub fn dir(data_dir: &Path, root: &RootRow) -> PathBuf {
    data_dir.join(&root.rel_path)
}

/// Runs a reconciliation scan of a root under its lock.
pub async fn scan(st: &AppState, root: &RootRow) -> ApiResult<super::scan::ScanReport> {
    let data_dir = st
        .cfg
        .data_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    // Changes a crash interrupted come first, so the scan sees their final state.
    super::ops::recover(st, Some(root.id)).await?;
    super::scan::scan_root(&st.db, &data_dir, root).await
}

/// Scans only some folders of a root under its lock (see [`super::scan::scan_dirs`]).
pub async fn scan_dirs(
    st: &AppState,
    root: &RootRow,
    dirs: &[PathBuf],
    delete_in: &std::collections::HashSet<PathBuf>,
) -> ApiResult<super::scan::PartialReport> {
    let data_dir = st
        .cfg
        .data_dir
        .clone()
        .ok_or_else(|| ApiError::Internal("Kein Datenverzeichnis konfiguriert".into()))?;
    let lock = st.root_lock(root.id);
    let _guard = lock.lock().await;
    super::ops::recover(st, Some(root.id)).await?;
    super::scan::scan_dirs(&st.db, &data_dir, root, dirs, delete_in).await
}

/// Scans all roots once (at startup: catches changes made while the server was not running).
pub async fn scan_all(st: &AppState) {
    let roots: Vec<RootRow> = match sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots ORDER BY id"
    )))
    .fetch_all(&st.db)
    .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "Ablagen nicht lesbar");
            return;
        }
    };
    for r in roots {
        // Watching starts first, so nothing changed during the scan is missed.
        if st.cfg.watch
            && let Err(e) = super::watch::start(st, &r).await
        {
            tracing::warn!(root = r.id, error = ?e, "Überwachung nicht gestartet");
        }
        if let Err(e) = scan(st, &r).await {
            tracing::warn!(root = r.id, error = ?e, "Abgleich fehlgeschlagen");
        }
    }
}

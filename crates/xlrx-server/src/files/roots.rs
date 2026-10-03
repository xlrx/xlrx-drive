//! Roots: which directories on the NAS xlrx manages, and who may see them.

use std::path::{Component, Path, PathBuf};

use super::access::Role;
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
    if let Some(other) = overlapping(&mut tx, &rel).await? {
        return Err(ApiError::Conflict(format!(
            "„Meine Ablage“ ({rel_str}) würde sich mit der Ablage „{other}“ überschneiden."
        )));
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

/// The name of a root lying inside `rel` or containing it, if any: roots never overlap (a file
/// would belong to two of them).
async fn overlapping(tx: &mut db::Tx, rel: &Path) -> ApiResult<Option<String>> {
    let roots: Vec<(String, String)> = sqlx::query_as("SELECT name, rel_path FROM roots")
        .fetch_all(&mut **tx)
        .await?;
    Ok(roots.into_iter().find_map(|(name, r)| {
        let r = Path::new(&r);
        (r.starts_with(rel) || rel.starts_with(r)).then_some(name)
    }))
}

/// Mounts an existing directory below the data directory as a shared root ("Geteilte Ablage",
/// PLAN 9.1), e.g. a Synology Drive team folder. Its members are set separately.
pub async fn mount_space(st: &AppState, name: &str, rel: &str, by: i64) -> ApiResult<RootRow> {
    let data_dir = st
        .cfg
        .data_dir
        .clone()
        .ok_or_else(|| ApiError::bad("Kein Datenverzeichnis eingerichtet."))?;
    let name = name.trim();
    if name.is_empty() || name.chars().any(char::is_control) || name.contains('/') {
        return Err(ApiError::bad(
            "Bitte einen Namen ohne Schrägstrich angeben.",
        ));
    }
    let rel = PathBuf::from(rel.trim().trim_matches('/'));
    if rel.as_os_str().is_empty() || !rel.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(ApiError::bad(
            "Der Pfad muss unterhalb des Datenverzeichnisses liegen, z. B. „Familie“.",
        ));
    }
    if let Some(state) = &st.cfg.state_dir
        && let Ok(state_rel) = state.strip_prefix(&data_dir)
        && (state_rel.starts_with(&rel) || rel.starts_with(state_rel))
    {
        return Err(ApiError::bad(
            "Das Verzeichnis von xlrx selbst kann keine Ablage sein.",
        ));
    }
    let dir = data_dir.join(&rel);
    // No symbolic links anywhere on the way: the root must really be this directory.
    let real = std::fs::canonicalize(&dir)
        .map_err(|_| ApiError::bad(format!("„{}“ gibt es nicht.", rel.display())))?;
    let base = std::fs::canonicalize(&data_dir).map_err(|e| ApiError::Internal(e.to_string()))?;
    let meta = std::fs::symlink_metadata(&dir).map_err(|e| ApiError::Internal(e.to_string()))?;
    if real != base.join(&rel) || !meta.is_dir() {
        return Err(ApiError::bad(format!(
            "„{}“ ist kein Ordner (oder eine Verknüpfung).",
            rel.display()
        )));
    }
    let (id, fp) = xlrx_chunk::fingerprint_of(&meta);
    let rel_str = rel.to_string_lossy().into_owned();
    let dir_name = rel
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_owned());
    let mut tx = db::begin_write(&st.db).await?;
    if let Some(other) = overlapping(&mut tx, &rel).await? {
        return Err(ApiError::Conflict(format!(
            "„{}“ überschneidet sich mit der Ablage „{other}“.",
            rel.display()
        )));
    }
    let root: RootRow = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO roots (kind, name, rel_path) VALUES ('space', $1, $2) RETURNING {ROOT_COLS}"
    )))
    .bind(name)
    .bind(&rel_str)
    .fetch_one(&mut *tx)
    .await?;
    db::insert_node(
        &mut tx,
        &NewNode {
            root_id: root.id,
            parent_id: None,
            name: &dir_name,
            is_dir: true,
            content: None,
            mtime: None,
            disk: Some(OnDisk { id, fp: Some(fp) }),
        },
        Source::api(by),
    )
    .await?;
    tx.commit().await?;
    tracing::info!(root = root.id, path = %rel_str, "Geteilte Ablage eingebunden");
    Ok(root)
}

pub async fn home(st: &AppState, user_id: i64) -> ApiResult<Option<RootRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots WHERE kind = 'home' AND owner_user_id = $1"
    )))
    .bind(user_id)
    .fetch_optional(&st.db)
    .await?)
}

/// The roots this person can see as a whole: the own home and the shared roots they are a member
/// of (directly or through a group). Items shared one by one are not included (see
/// [`super::access::scope`]).
pub async fn readable(st: &AppState, user_id: i64) -> ApiResult<Vec<RootRow>> {
    let ids: Vec<i64> = super::access::root_roles(&st.db, user_id)
        .await?
        .into_keys()
        .collect();
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots WHERE id = ANY($1) ORDER BY kind, id"
    )))
    .bind(&ids)
    .fetch_all(&st.db)
    .await?)
}

/// The person's role over a whole root (owner or member), if any.
pub async fn role(st: &AppState, root: &RootRow, user_id: i64) -> ApiResult<Option<Role>> {
    Ok(super::access::root_roles(&st.db, user_id)
        .await?
        .get(&root.id)
        .copied())
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

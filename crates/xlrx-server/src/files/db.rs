//! Nodes and journal in the database.

use std::collections::HashMap;
use std::path::PathBuf;

use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use xlrx_chunk::{FileId, Fingerprint};
use xlrx_proto::Name;

use crate::error::ApiResult;

/// Advisory lock taken by every transaction that writes the journal: sequence numbers then
/// become visible in commit order.
const JOURNAL_LOCK: i64 = 0x786c_7278_6a6e_6c00;

pub type Tx = Transaction<'static, Postgres>;

pub async fn begin_write(db: &PgPool) -> ApiResult<Tx> {
    let mut tx = db.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(JOURNAL_LOCK)
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct RootRow {
    pub id: i64,
    pub kind: String,
    pub name: String,
    pub rel_path: String,
    pub owner_user_id: Option<i64>,
    pub scanned_at: Option<OffsetDateTime>,
}

pub const ROOT_COLS: &str = "id, kind, name, rel_path, owner_user_id, scanned_at";

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct NodeRow {
    pub id: i64,
    pub root_id: i64,
    pub parent_id: Option<i64>,
    pub name: String,
    pub kind: String,
    pub content_hash: Option<Vec<u8>>,
    pub size: Option<i64>,
    pub rev: i64,
    pub seq: i64,
    pub mtime: Option<OffsetDateTime>,
    pub fs_dev: Option<i64>,
    pub fs_ino: Option<i64>,
    pub fs_size: Option<i64>,
    pub fs_mtime_ns: Option<i64>,
    pub fs_ctime_ns: Option<i64>,
    pub deleted_at: Option<OffsetDateTime>,
}

pub const NODE_COLS: &str = "id, root_id, parent_id, name, kind, content_hash, size, rev, seq, \
    mtime, fs_dev, fs_ino, fs_size, fs_mtime_ns, fs_ctime_ns, deleted_at";

impl NodeRow {
    pub fn is_dir(&self) -> bool {
        self.kind == "dir"
    }

    pub fn file_id(&self) -> Option<FileId> {
        Some(FileId {
            dev: self.fs_dev? as u64,
            ino: self.fs_ino? as u64,
        })
    }

    pub fn fingerprint(&self) -> Option<Fingerprint> {
        Some(Fingerprint {
            size: self.fs_size? as u64,
            mtime_ns: self.fs_mtime_ns?,
            ctime_ns: self.fs_ctime_ns?,
        })
    }
}

/// Comparison key of a name (case-insensitive, NFC).
pub fn fold(name: &str) -> String {
    Name::new(name)
        .map(|n| n.fold_key())
        .unwrap_or_else(|_| name.to_lowercase())
}

/// Identity and fingerprint on disk as stored in `nodes`.
#[derive(Clone, Copy, Debug)]
pub struct OnDisk {
    pub id: FileId,
    /// `None` if the stored hash must not be trusted for this fingerprint (modified too close to
    /// hashing, see [`xlrx_chunk::RACY_WINDOW_NS`]); the next scan then hashes again.
    pub fp: Option<Fingerprint>,
}

pub struct NewNode<'a> {
    pub root_id: i64,
    pub parent_id: Option<i64>,
    pub name: &'a str,
    pub is_dir: bool,
    /// Files: (hash, size).
    pub content: Option<([u8; 32], u64)>,
    pub mtime: Option<OffsetDateTime>,
    pub disk: Option<OnDisk>,
}

/// Who caused a change.
#[derive(Clone, Copy, Debug)]
pub struct Source {
    pub api: bool,
    pub actor: Option<i64>,
}

impl Source {
    pub const SCAN: Source = Source {
        api: false,
        actor: None,
    };

    pub fn api(actor: i64) -> Self {
        Self {
            api: true,
            actor: Some(actor),
        }
    }

    fn name(&self) -> &'static str {
        if self.api { "api" } else { "scan" }
    }
}

/// Inserts a node together with its `create` journal entry. Returns (id, seq).
pub async fn insert_node(tx: &mut Tx, n: &NewNode<'_>, src: Source) -> ApiResult<(i64, i64)> {
    let row: (i64, i64) = sqlx::query_as(
        "WITH s AS (SELECT nextval('journal_seq') AS seq),
         n AS (
           INSERT INTO nodes (root_id, parent_id, name, name_folded, kind, content_hash, size,
                              rev, seq, mtime, fs_dev, fs_ino, fs_size, fs_mtime_ns, fs_ctime_ns)
           SELECT $1, $2, $3, $4, $5, $6, $7, s.seq, s.seq, $8, $9, $10, $11, $12, $13 FROM s
           RETURNING id, seq)
         INSERT INTO journal (seq, root_id, node_id, op, source, actor_user_id)
         SELECT seq, $1, id, 'create', $14, $15 FROM n
         RETURNING node_id, seq",
    )
    .bind(n.root_id)
    .bind(n.parent_id)
    .bind(n.name)
    .bind(fold(n.name))
    .bind(if n.is_dir { "dir" } else { "file" })
    .bind(n.content.map(|c| c.0.to_vec()))
    .bind(n.content.map(|c| c.1 as i64))
    .bind(n.mtime)
    .bind(n.disk.map(|d| d.id.dev as i64))
    .bind(n.disk.map(|d| d.id.ino as i64))
    .bind(n.disk.and_then(|d| d.fp).map(|f| f.size as i64))
    .bind(n.disk.and_then(|d| d.fp).map(|f| f.mtime_ns))
    .bind(n.disk.and_then(|d| d.fp).map(|f| f.ctime_ns))
    .bind(src.name())
    .bind(src.actor)
    .fetch_one(&mut **tx)
    .await?;
    Ok(row)
}

/// Writes a journal entry for an existing node and returns its sequence number.
pub async fn journal(
    tx: &mut Tx,
    root_id: i64,
    node_id: i64,
    op: &str,
    src: Source,
) -> ApiResult<i64> {
    let (seq,): (i64,) = sqlx::query_as(
        "INSERT INTO journal (root_id, node_id, op, source, actor_user_id)
         VALUES ($1, $2, $3, $4, $5) RETURNING seq",
    )
    .bind(root_id)
    .bind(node_id)
    .bind(op)
    .bind(src.name())
    .bind(src.actor)
    .fetch_one(&mut **tx)
    .await?;
    Ok(seq)
}

pub async fn set_location(
    tx: &mut Tx,
    node: &NodeRow,
    parent_id: i64,
    name: &str,
    src: Source,
) -> ApiResult<i64> {
    let seq = journal(tx, node.root_id, node.id, "move", src).await?;
    sqlx::query(
        "UPDATE nodes SET parent_id = $2, name = $3, name_folded = $4, seq = $5, updated_at = now()
         WHERE id = $1",
    )
    .bind(node.id)
    .bind(parent_id)
    .bind(name)
    .bind(fold(name))
    .bind(seq)
    .execute(&mut **tx)
    .await?;
    Ok(seq)
}

pub async fn set_content(
    tx: &mut Tx,
    node: &NodeRow,
    hash: [u8; 32],
    size: u64,
    mtime: Option<OffsetDateTime>,
    disk: Option<OnDisk>,
    src: Source,
) -> ApiResult<i64> {
    let seq = journal(tx, node.root_id, node.id, "update", src).await?;
    sqlx::query(
        "UPDATE nodes SET content_hash = $2, size = $3, mtime = $4, rev = $5, seq = $5,
                fs_dev = $6, fs_ino = $7, fs_size = $8, fs_mtime_ns = $9, fs_ctime_ns = $10,
                updated_at = now()
         WHERE id = $1",
    )
    .bind(node.id)
    .bind(hash.to_vec())
    .bind(size as i64)
    .bind(mtime)
    .bind(seq)
    .bind(disk.map(|d| d.id.dev as i64))
    .bind(disk.map(|d| d.id.ino as i64))
    .bind(disk.and_then(|d| d.fp).map(|f| f.size as i64))
    .bind(disk.and_then(|d| d.fp).map(|f| f.mtime_ns))
    .bind(disk.and_then(|d| d.fp).map(|f| f.ctime_ns))
    .execute(&mut **tx)
    .await?;
    Ok(seq)
}

/// Updates only identity/fingerprint on disk (no visible change, no journal entry).
pub async fn set_disk(
    tx: &mut Tx,
    node_id: i64,
    disk: OnDisk,
    mtime: Option<OffsetDateTime>,
) -> ApiResult<()> {
    sqlx::query(
        "UPDATE nodes SET fs_dev = $2, fs_ino = $3, fs_size = $4, fs_mtime_ns = $5, fs_ctime_ns = $6,
                mtime = COALESCE($7, mtime)
         WHERE id = $1",
    )
    .bind(node_id)
    .bind(disk.id.dev as i64)
    .bind(disk.id.ino as i64)
    .bind(disk.fp.map(|f| f.size as i64))
    .bind(disk.fp.map(|f| f.mtime_ns))
    .bind(disk.fp.map(|f| f.ctime_ns))
    .bind(mtime)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn mark_deleted(
    tx: &mut Tx,
    node: &NodeRow,
    trash_path: Option<&str>,
    src: Source,
) -> ApiResult<i64> {
    let seq = journal(tx, node.root_id, node.id, "delete", src).await?;
    sqlx::query(
        "UPDATE nodes SET deleted_at = now(), deleted_by = $2, trash_path = $3, seq = $4,
                updated_at = now()
         WHERE id = $1",
    )
    .bind(node.id)
    .bind(src.actor)
    .bind(trash_path)
    .bind(seq)
    .execute(&mut **tx)
    .await?;
    Ok(seq)
}

pub async fn root_by_id(db: &PgPool, id: i64) -> ApiResult<Option<RootRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROOT_COLS} FROM roots WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

pub async fn node_by_id(db: &PgPool, id: i64) -> ApiResult<Option<NodeRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// All live nodes of a root.
pub async fn live_nodes(db: &PgPool, root_id: i64) -> ApiResult<Vec<NodeRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE root_id = $1 AND deleted_at IS NULL"
    )))
    .bind(root_id)
    .fetch_all(db)
    .await?)
}

/// The root directory node of a root.
pub async fn root_node(db: &PgPool, root_id: i64) -> ApiResult<Option<NodeRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE root_id = $1 AND parent_id IS NULL AND deleted_at IS NULL"
    )))
    .bind(root_id)
    .fetch_optional(db)
    .await?)
}

/// Ancestors of a node from the root directory down to the node itself.
pub async fn ancestors(db: &PgPool, id: i64) -> ApiResult<Vec<NodeRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE up AS (
           SELECT {NODE_COLS}, 0 AS depth FROM nodes WHERE id = $1
           UNION ALL
           SELECT n.id, n.root_id, n.parent_id, n.name, n.kind, n.content_hash, n.size, n.rev,
                  n.seq, n.mtime, n.fs_dev, n.fs_ino, n.fs_size, n.fs_mtime_ns, n.fs_ctime_ns,
                  n.deleted_at, up.depth + 1
           FROM nodes n JOIN up ON n.id = up.parent_id WHERE up.depth < 1000)
         SELECT {NODE_COLS} FROM up ORDER BY depth DESC"
    )))
    .bind(id)
    .fetch_all(db)
    .await?)
}

/// Path of a node relative to its root directory (empty for the root node).
pub async fn rel_path(db: &PgPool, id: i64) -> ApiResult<PathBuf> {
    let chain = ancestors(db, id).await?;
    Ok(chain.iter().skip(1).map(|n| n.name.as_str()).collect())
}

/// Relative paths of all nodes in `nodes` (which must contain complete parent chains).
pub fn paths(nodes: &[NodeRow]) -> HashMap<i64, PathBuf> {
    let by_id: HashMap<i64, &NodeRow> = nodes.iter().map(|n| (n.id, n)).collect();
    let mut out: HashMap<i64, PathBuf> = HashMap::with_capacity(nodes.len());
    for n in nodes {
        if out.contains_key(&n.id) {
            continue;
        }
        // Walk up to a known path, then fill in on the way back down.
        let mut chain = vec![n.id];
        let mut cur = n.parent_id;
        let mut guard = 0;
        while let Some(p) = cur {
            if out.contains_key(&p) || guard > 10_000 {
                break;
            }
            chain.push(p);
            cur = by_id.get(&p).and_then(|x| x.parent_id);
            guard += 1;
        }
        for id in chain.into_iter().rev() {
            let Some(node) = by_id.get(&id) else { continue };
            let path = match node.parent_id {
                None => PathBuf::new(),
                Some(p) => out.get(&p).cloned().unwrap_or_default().join(&node.name),
            };
            out.insert(id, path);
        }
    }
    out
}

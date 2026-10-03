//! Who may do what (PLAN 9.1, 9.3).
//!
//! A person's role on a node is the highest of:
//! - owner of the home root the node is in,
//! - member of the shared root ("Geteilte Ablage") the node is in,
//! - a share on the node or any folder above it, to the person or one of their groups, not
//!   expired.
//!
//! Nobody else sees the node – administrators included. Callers answer "not found" when there is
//! no role at all, so the existence of other people's files is never revealed.

use std::collections::{HashMap, HashSet};

use serde::Serialize;
use sqlx::PgPool;

use super::db::{self, NodeRow, RootRow};
use crate::error::{ApiError, ApiResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Ansehen: browse, preview, download.
    Viewer = 1,
    /// Bearbeiten: also upload, change, rename, move and delete inside.
    Editor = 2,
    /// Verwalten: also share further.
    Manager = 3,
    /// Owner of a home root.
    Owner = 4,
}

impl Role {
    /// The roles that can be given (owner cannot).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "viewer" => Some(Role::Viewer),
            "editor" => Some(Role::Editor),
            "manager" => Some(Role::Manager),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Viewer => "viewer",
            Role::Editor => "editor",
            Role::Manager => "manager",
            Role::Owner => "owner",
        }
    }

    fn from_rank(r: i32) -> Option<Self> {
        match r {
            1 => Some(Role::Viewer),
            2 => Some(Role::Editor),
            3 => Some(Role::Manager),
            4 => Some(Role::Owner),
            _ => None,
        }
    }
}

/// SQL rank of a stored role.
const RANK: &str = "CASE role WHEN 'viewer' THEN 1 WHEN 'editor' THEN 2 WHEN 'manager' THEN 3 END";

/// Roles over whole roots: owner of a home, member of a shared root (directly or by group).
pub async fn root_roles(db: &PgPool, user_id: i64) -> ApiResult<HashMap<i64, Role>> {
    let rows: Vec<(i64, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT id, 4 FROM roots WHERE kind = 'home' AND owner_user_id = $1
         UNION ALL
         SELECT root_id, {RANK} FROM root_members
          WHERE user_id = $1
             OR group_id IN (SELECT group_id FROM group_members WHERE user_id = $1)"
    )))
    .bind(user_id)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<i64, Role> = HashMap::new();
    for (root, rank) in rows {
        if let Some(r) = Role::from_rank(rank) {
            let e = out.entry(root).or_insert(r);
            *e = (*e).max(r);
        }
    }
    Ok(out)
}

/// Roles from shares on the given nodes or the folders above them.
pub async fn share_roles(db: &PgPool, user_id: i64, ids: &[i64]) -> ApiResult<HashMap<i64, Role>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, i32)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "WITH RECURSIVE up AS (
            SELECT id AS start, id, parent_id, 0 AS depth FROM nodes WHERE id = ANY($1)
            UNION ALL
            SELECT up.start, n.id, n.parent_id, up.depth + 1
              FROM nodes n JOIN up ON n.id = up.parent_id WHERE up.depth < 1000
         )
         SELECT up.start, max({RANK}) FROM up JOIN shares s ON s.node_id = up.id
          WHERE (s.expires_at IS NULL OR s.expires_at > now())
            AND (s.user_id = $2
                 OR s.group_id IN (SELECT group_id FROM group_members WHERE user_id = $2))
          GROUP BY up.start"
    )))
    .bind(ids)
    .bind(user_id)
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, rank)| Some((id, Role::from_rank(rank)?)))
        .collect())
}

/// The person's role on each of the nodes (missing: no access).
pub async fn roles(db: &PgPool, user_id: i64, nodes: &[NodeRow]) -> ApiResult<HashMap<i64, Role>> {
    let by_root = root_roles(db, user_id).await?;
    let ids: Vec<i64> = nodes.iter().map(|n| n.id).collect();
    let shared = share_roles(db, user_id, &ids).await?;
    Ok(nodes
        .iter()
        .filter_map(|n| {
            let r = by_root
                .get(&n.root_id)
                .copied()
                .max(shared.get(&n.id).copied())?;
            Some((n.id, r))
        })
        .collect())
}

/// A node (live or in the trash) with its root and the person's role on it.
pub struct Access {
    pub node: NodeRow,
    pub root: RootRow,
    pub role: Role,
    /// Role over the whole root (owner or member), if any.
    pub root_role: Option<Role>,
}

/// The node if the person has any role on it.
pub async fn of(db: &PgPool, user_id: i64, id: i64) -> ApiResult<Option<Access>> {
    let Some(node) = db::node_by_id(db, id).await? else {
        return Ok(None);
    };
    let Some(root) = db::root_by_id(db, node.root_id).await? else {
        return Ok(None);
    };
    let root_role = root_roles(db, user_id).await?.get(&root.id).copied();
    let shared = share_roles(db, user_id, &[node.id])
        .await?
        .get(&node.id)
        .copied();
    Ok(root_role.max(shared).map(|role| Access {
        node,
        root,
        role,
        root_role,
    }))
}

/// The node if the person has at least `min` on it. No role at all: "not found"; too little:
/// forbidden.
pub async fn require(db: &PgPool, user_id: i64, id: i64, min: Role) -> ApiResult<Access> {
    let a = of(db, user_id, id).await?.ok_or(ApiError::NotFound)?;
    if a.role < min {
        return Err(ApiError::forbidden(match min {
            Role::Viewer => "Kein Zugriff.",
            Role::Editor => "Du darfst hier nur ansehen.",
            Role::Manager | Role::Owner => "Nur wer das verwaltet, darf es teilen.",
        }));
    }
    Ok(a)
}

/// The node, if the person may change what is in the folder it lies in (create, rename, move,
/// delete). A shared item itself can therefore only be renamed or deleted by someone with rights
/// on the folder above it, never by the people it is shared with.
pub async fn require_in_parent(db: &PgPool, user_id: i64, id: i64) -> ApiResult<Access> {
    let a = of(db, user_id, id).await?.ok_or(ApiError::NotFound)?;
    let parent_role = match a.node.parent_id {
        Some(p) => of(db, user_id, p).await?.map(|p| p.role),
        None => a.root_role,
    };
    if parent_role.is_none_or(|r| r < Role::Editor) {
        return Err(ApiError::forbidden(if a.role >= Role::Editor {
            "Nur wer den Ordner darüber bearbeiten darf, kann das umbenennen, verschieben oder löschen."
        } else {
            "Du darfst hier nur ansehen."
        }));
    }
    Ok(a)
}

/// What a person can see: whole roots, and single nodes shared with them outside those roots.
/// The search index and the activity stream filter by this (PLAN 9.3).
#[derive(Clone, Debug, Default)]
pub struct Scope {
    pub roots: Vec<i64>,
    /// Live shared nodes (files or folders, everything below included).
    pub shared: Vec<i64>,
}

pub async fn scope(db: &PgPool, user_id: i64) -> ApiResult<Scope> {
    let mut roots: Vec<i64> = root_roles(db, user_id).await?.into_keys().collect();
    roots.sort_unstable();
    let shared: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT s.node_id FROM shares s JOIN nodes n ON n.id = s.node_id
          WHERE n.deleted_at IS NULL AND NOT (n.root_id = ANY($2))
            AND (s.expires_at IS NULL OR s.expires_at > now())
            AND (s.user_id = $1
                 OR s.group_id IN (SELECT group_id FROM group_members WHERE user_id = $1))
          ORDER BY 1",
    )
    .bind(user_id)
    .bind(&roots)
    .fetch_all(db)
    .await?;
    Ok(Scope { roots, shared })
}

/// The live nodes among `ids` the person may see (second check after the search index).
pub async fn visible_ids(db: &PgPool, user_id: i64, ids: &[i64]) -> ApiResult<HashSet<i64>> {
    let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM nodes WHERE id = ANY($1) AND deleted_at IS NULL",
        db::NODE_COLS
    )))
    .bind(ids)
    .fetch_all(db)
    .await?;
    Ok(roles(db, user_id, &nodes).await?.into_keys().collect())
}

/// The folders from the highest one the person can see down to each node (the node itself
/// included): from the root directory with whole-root access, else from the topmost folder (or
/// file) shared with them. Folders above are never revealed.
pub async fn chains(
    db: &PgPool,
    scope: &Scope,
    ids: &[i64],
) -> ApiResult<HashMap<i64, Vec<(i64, String)>>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows: Vec<(i64, i64, String, i64, i32)> = sqlx::query_as(
        "WITH RECURSIVE up AS (
            SELECT id AS start, id, name, root_id, parent_id, 0 AS depth FROM nodes WHERE id = ANY($1)
            UNION ALL
            SELECT up.start, n.id, n.name, n.root_id, n.parent_id, up.depth + 1
              FROM nodes n JOIN up ON n.id = up.parent_id WHERE up.depth < 1000
         )
         SELECT start, id, name, root_id, depth FROM up ORDER BY start, depth DESC",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    let shared: HashSet<i64> = scope.shared.iter().copied().collect();
    let mut out: HashMap<i64, Vec<(i64, String)>> = HashMap::new();
    let mut full: HashMap<i64, bool> = HashMap::new();
    for (start, id, name, root_id, _) in rows {
        let whole = *full
            .entry(start)
            .or_insert_with(|| scope.roots.contains(&root_id));
        let chain = out.entry(start).or_default();
        if whole || !chain.is_empty() || shared.contains(&id) {
            chain.push((id, name));
        }
    }
    Ok(out)
}

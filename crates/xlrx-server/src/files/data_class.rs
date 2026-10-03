//! Data classes (PLAN 7.4): may the content of a folder be processed outside the home network?
//!
//! - **local** ("Nur lokal"): everything stays on the NAS – for health data, contracts, data of
//!   third parties. Legal reasons: no cloud service, not even an own rented server.
//! - **cloud** ("Cloud erlaubt"): AI analysis by a provider in the EU may be used (M4).
//!
//! A setting on a folder applies to everything below it unless a folder further down says
//! otherwise. Without any setting the server's default applies, "local" unless configured
//! otherwise. Every path out of the house must call [`allows_cloud`] first.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::ApiResult;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Cloud,
    Local,
}

impl Class {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "cloud" => Some(Class::Cloud),
            "local" => Some(Class::Local),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Class::Cloud => "cloud",
            Class::Local => "local",
        }
    }
}

/// The class a node has, and where it comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Effective {
    pub class: Class,
    /// The node carrying the setting (the node itself if set there); `None`: server default.
    pub from: Option<i64>,
}

/// The classes of the given nodes (deleted ones included: they keep their place).
pub async fn effective(
    db: &PgPool,
    default: Class,
    ids: &[i64],
) -> ApiResult<HashMap<i64, Effective>> {
    let rows: Vec<(i64, String, i64)> = sqlx::query_as(
        "WITH RECURSIVE up AS (
            SELECT id AS start, id, parent_id, 0 AS depth FROM nodes WHERE id = ANY($1)
            UNION ALL
            SELECT up.start, n.id, n.parent_id, up.depth + 1
              FROM nodes n JOIN up ON n.id = up.parent_id WHERE up.depth < 1000
         )
         SELECT DISTINCT ON (up.start) up.start, d.class, d.node_id
           FROM up JOIN data_classes d ON d.node_id = up.id
          ORDER BY up.start, up.depth",
    )
    .bind(ids)
    .fetch_all(db)
    .await?;
    let mut out: HashMap<i64, Effective> = ids
        .iter()
        .map(|&id| {
            (
                id,
                Effective {
                    class: default,
                    from: None,
                },
            )
        })
        .collect();
    for (start, class, from) in rows {
        // An unknown value in the database counts as the safe one.
        let class = Class::parse(&class).unwrap_or(Class::Local);
        out.insert(
            start,
            Effective {
                class,
                from: Some(from),
            },
        );
    }
    Ok(out)
}

/// May the content of this node leave the home network? Every cloud path asks this first.
pub async fn allows_cloud(db: &PgPool, default: Class, id: i64) -> ApiResult<bool> {
    Ok(effective(db, default, &[id])
        .await?
        .get(&id)
        .is_some_and(|e| e.class == Class::Cloud))
}

/// Contents that may not leave the home network (for every cloud path that sends contents: the
/// outside cache, M4): a copy that still exists – a file, one in the trash, an old version – lies in
/// a "Nur lokal" folder, or no live file in a "Cloud erlaubt" folder has it.
pub async fn forbidden_contents(
    db: &PgPool,
    default: Class,
    hashes: &[Vec<u8>],
) -> ApiResult<HashSet<Vec<u8>>> {
    if hashes.is_empty() {
        return Ok(HashSet::new());
    }
    let rows: Vec<(i64, Vec<u8>, bool)> = sqlx::query_as(
        "SELECT n.id, n.content_hash, n.deleted_at IS NULL FROM nodes n
          WHERE n.content_hash = ANY($1)
            AND (n.deleted_at IS NULL OR EXISTS (
                  SELECT 1 FROM nodes t WHERE t.id = n.deleted_with AND t.trash_path IS NOT NULL))
         UNION ALL
         SELECT node_id, content_hash, false FROM versions WHERE content_hash = ANY($1)",
    )
    .bind(hashes)
    .fetch_all(db)
    .await?;
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.0)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let classes = effective(db, default, &ids).await?;
    let mut local = HashSet::new();
    let mut allowed = HashSet::new();
    for (id, hash, live) in rows {
        // Unknown means local: only an explicit "Cloud erlaubt" lets a content out.
        match classes.get(&id).map(|e| e.class) {
            Some(Class::Cloud) if live => {
                allowed.insert(hash);
            }
            Some(Class::Cloud) => {}
            _ => {
                local.insert(hash);
            }
        }
    }
    Ok(hashes
        .iter()
        .filter(|h| local.contains(*h) || !allowed.contains(*h))
        .cloned()
        .collect())
}

/// The settings made on these nodes themselves.
pub async fn explicit(db: &PgPool, ids: &[i64]) -> ApiResult<HashMap<i64, Class>> {
    let rows: Vec<(i64, String)> =
        sqlx::query_as("SELECT node_id, class FROM data_classes WHERE node_id = ANY($1)")
            .bind(ids)
            .fetch_all(db)
            .await?;
    Ok(rows
        .into_iter()
        .map(|(id, c)| (id, Class::parse(&c).unwrap_or(Class::Local)))
        .collect())
}

/// Sets (or with `None` removes) the class of a folder.
pub async fn set(db: &PgPool, node_id: i64, class: Option<Class>, by: i64) -> ApiResult<()> {
    match class {
        Some(c) => {
            sqlx::query(
                "INSERT INTO data_classes (node_id, class, set_by) VALUES ($1, $2, $3)
                 ON CONFLICT (node_id) DO UPDATE
                    SET class = EXCLUDED.class, set_by = EXCLUDED.set_by, set_at = now()",
            )
            .bind(node_id)
            .bind(c.as_str())
            .bind(by)
            .execute(db)
            .await?;
        }
        None => {
            sqlx::query("DELETE FROM data_classes WHERE node_id = $1")
                .bind(node_id)
                .execute(db)
                .await?;
        }
    }
    Ok(())
}

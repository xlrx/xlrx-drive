//! Activity (PLAN 8.3): what happened, grouped by person, action, folder and half an hour –
//! "Anna hat 23 Fotos zu Urlaub hinzugefügt".
//!
//! Changes to files come straight from the journal: who (or "on the NAS" for changes found by a
//! scan), what, when. What a folder's deletion or restoration took along (same transaction) shows
//! as the folder alone, and the first import of a root is not news. Sharing and public links come
//! from `events`. Everyone sees only what happened to items they may see now; the use of public
//! links only those who manage the item.

use std::collections::{HashMap, HashSet};

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;

use super::files::{NodeInfo, folder_names};
use crate::auth::session::CurrentUser;
use crate::error::ApiResult;
use crate::files::access::{self, Role};
use crate::files::db::{NODE_COLS, NodeRow};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A folder was made.
    Created,
    /// A file was added.
    Uploaded,
    Edited,
    Renamed,
    Moved,
    Deleted,
    Restored,
    Shared,
    LinkCreated,
    LinkDownload,
    LinkUpload,
    LinkEdit,
}

impl Kind {
    pub fn sharing(self) -> bool {
        matches!(
            self,
            Kind::Shared
                | Kind::LinkCreated
                | Kind::LinkDownload
                | Kind::LinkUpload
                | Kind::LinkEdit
        )
    }

    fn links(self) -> bool {
        matches!(
            self,
            Kind::LinkCreated | Kind::LinkDownload | Kind::LinkUpload | Kind::LinkEdit
        )
    }
}

/// Who did it when there is no person: found on the NAS (SMB, Synology Drive …) or someone
/// through a public link.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    Nas,
    Link,
}

struct Row {
    at: OffsetDateTime,
    actor: Option<i64>,
    via: Option<Via>,
    kind: Kind,
    node_id: i64,
    prev_name: Option<String>,
    details: Value,
}

#[derive(sqlx::FromRow)]
struct JournalRow {
    at: OffsetDateTime,
    actor_user_id: Option<i64>,
    op: String,
    source: String,
    reparented: Option<bool>,
    prev_name: Option<String>,
    node_id: i64,
    node_kind: String,
}

#[derive(sqlx::FromRow)]
struct EventRow {
    at: OffsetDateTime,
    actor_user_id: Option<i64>,
    kind: String,
    node_id: i64,
    details: Value,
}

/// The newest `limit` rows before `before` in these roots (or of one node).
async fn rows(
    st: &AppState,
    roots: &[i64],
    node: Option<i64>,
    before: OffsetDateTime,
    limit: i64,
) -> ApiResult<Vec<Row>> {
    let journal: Vec<JournalRow> = sqlx::query_as(
        "SELECT j.at, j.actor_user_id, j.op, j.source, j.reparented, j.prev_name, j.node_id,
                n.kind AS node_kind
           FROM journal j
           JOIN nodes n ON n.id = j.node_id
           JOIN roots r ON r.id = j.root_id
          WHERE j.root_id = ANY($1) AND j.at < $2 AND n.parent_id IS NOT NULL
            AND ($3::bigint IS NULL OR j.node_id = $3)
            -- The first import is not news.
            AND NOT (j.source = 'scan' AND (r.first_scanned_at IS NULL OR j.at <= r.first_scanned_at))
            AND NOT (j.op = 'update' AND n.kind = 'dir')
            -- What a folder took along in the same transaction shows as the folder alone.
            AND NOT (j.op IN ('delete', 'restore') AND EXISTS (
                  SELECT 1 FROM journal p
                   WHERE p.node_id = n.parent_id AND p.op = j.op AND p.at = j.at))
            -- Added through a public link: shown as that (`events`), not as the link's creator.
            AND NOT (j.op IN ('create', 'update') AND EXISTS (
                  SELECT 1 FROM events e
                   WHERE e.node_id = j.node_id AND e.kind IN ('link_upload', 'link_edit')
                     AND e.at >= j.at AND e.at < j.at + interval '1 minute'))
          ORDER BY j.at DESC, j.seq DESC
          LIMIT $4",
    )
    .bind(roots)
    .bind(before)
    .bind(node)
    .bind(limit)
    .fetch_all(&st.db)
    .await?;
    let events: Vec<EventRow> = sqlx::query_as(
        "SELECT e.at, e.actor_user_id, e.kind, e.node_id, e.details
           FROM events e JOIN nodes n ON n.id = e.node_id
          WHERE n.root_id = ANY($1) AND e.at < $2 AND ($3::bigint IS NULL OR e.node_id = $3)
          ORDER BY e.at DESC, e.id DESC
          LIMIT $4",
    )
    .bind(roots)
    .bind(before)
    .bind(node)
    .bind(limit)
    .fetch_all(&st.db)
    .await?;
    let mut out: Vec<Row> = journal
        .into_iter()
        .filter_map(|j| {
            let kind = match j.op.as_str() {
                "create" if j.node_kind == "dir" => Kind::Created,
                "create" => Kind::Uploaded,
                "update" => Kind::Edited,
                "move" if j.reparented == Some(true) => Kind::Moved,
                "move" => Kind::Renamed,
                "delete" => Kind::Deleted,
                "restore" => Kind::Restored,
                _ => return None,
            };
            let nas = j.source == "scan";
            Some(Row {
                at: j.at,
                actor: if nas { None } else { j.actor_user_id },
                via: nas.then_some(Via::Nas),
                kind,
                node_id: j.node_id,
                prev_name: j.prev_name,
                details: Value::Null,
            })
        })
        .collect();
    out.extend(events.into_iter().filter_map(|e| {
        let (kind, via) = match e.kind.as_str() {
            "shared" => (Kind::Shared, None),
            "link_created" => (Kind::LinkCreated, None),
            "link_download" => (Kind::LinkDownload, Some(Via::Link)),
            "link_upload" => (Kind::LinkUpload, Some(Via::Link)),
            "link_edit" => (Kind::LinkEdit, Some(Via::Link)),
            _ => return None,
        };
        Some(Row {
            at: e.at,
            actor: e.actor_user_id,
            via,
            kind,
            node_id: e.node_id,
            prev_name: None,
            details: e.details,
        })
    }));
    out.sort_by_key(|r| std::cmp::Reverse(r.at));
    out.truncate(limit as usize);
    Ok(out)
}

#[derive(Serialize, Clone)]
pub struct Person {
    pub id: i64,
    pub name: String,
}

#[derive(Serialize)]
pub struct Item {
    #[serde(flatten)]
    pub node: NodeInfo,
    pub deleted: bool,
    /// The name before a rename.
    pub prev_name: Option<String>,
}

#[derive(Serialize)]
pub struct Group {
    pub kind: Kind,
    pub actor: Option<Person>,
    pub via: Option<Via>,
    /// Done by the person asking.
    pub mine: bool,
    /// Newest and oldest moment in the group.
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
    /// Different items (the first few are listed).
    pub count: usize,
    pub items: Vec<Item>,
    /// Where they are, as far as the person may see it ("Meine Ablage/Urlaub").
    pub folder: Option<String>,
    /// The folder, if the person may open it.
    pub folder_id: Option<i64>,
    /// Of the newest entry: with whom something was shared, which kind of link.
    pub details: Value,
}

/// Items listed per group.
const ITEMS: usize = 8;
/// Entries within this time are one group.
const WINDOW_SECS: i64 = 30 * 60;

/// What makes entries one group: action, person (or how), folder.
type GroupKey = (Kind, Option<i64>, Option<Via>, Option<i64>);

#[derive(Deserialize)]
pub struct ActivityQuery {
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub before: Option<OffsetDateTime>,
    /// `all` (default), `others` (not by me), `shares` (sharing and links).
    pub who: Option<String>,
    /// Only this item (its history in the details).
    pub node: Option<i64>,
    /// Entries to look at (default 300).
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct ActivityPage {
    pub groups: Vec<Group>,
    /// For the next page (`before`), if there may be more.
    #[serde(with = "time::serde::rfc3339::option")]
    pub next: Option<OffsetDateTime>,
}

pub async fn activity(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<ActivityQuery>,
) -> ApiResult<Json<ActivityPage>> {
    let limit = q.limit.unwrap_or(300).clamp(10, 1000);
    let before = q
        .before
        .unwrap_or_else(|| OffsetDateTime::now_utc() + time::Duration::minutes(1));
    let scope = access::scope(&st.db, me.id).await?;
    let roots: Vec<i64> = match q.node {
        Some(id) => vec![
            access::require(&st.db, me.id, id, Role::Viewer)
                .await?
                .root
                .id,
        ],
        None => {
            let mut r = scope.roots.clone();
            r.extend(
                sqlx::query_scalar::<_, i64>(
                    "SELECT DISTINCT root_id FROM nodes WHERE id = ANY($1)",
                )
                .bind(&scope.shared)
                .fetch_all(&st.db)
                .await?,
            );
            r
        }
    };
    let rows = rows(&st, &roots, q.node, before, limit).await?;
    let next = (rows.len() as i64 == limit)
        .then(|| rows.last().map(|r| r.at))
        .flatten();

    // What the person may see: the items, and the folders they are in.
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| r.node_id)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes
          WHERE id = ANY($1) OR id IN (SELECT parent_id FROM nodes WHERE id = ANY($1))"
    )))
    .bind(&ids)
    .fetch_all(&st.db)
    .await?;
    let roles = access::roles(&st.db, me.id, &nodes).await?;
    let by_id: HashMap<i64, &NodeRow> = nodes.iter().map(|n| (n.id, n)).collect();
    let item_rows: Vec<NodeRow> = ids
        .iter()
        .filter_map(|id| by_id.get(id).map(|n| (*n).clone()))
        .collect();
    let folders = folder_names(&st, &scope, &item_rows).await?;

    let who = q.who.as_deref().unwrap_or("all");
    let mut groups: Vec<Group> = Vec::new();
    // Open group per (kind, actor, via, folder): index into `groups`.
    let mut open: HashMap<GroupKey, usize> = HashMap::new();
    let mut seen: HashMap<usize, HashSet<i64>> = HashMap::new();
    for r in rows {
        let Some(node) = by_id.get(&r.node_id) else {
            continue;
        };
        let Some(role) = roles.get(&r.node_id) else {
            continue;
        };
        if r.kind.links() && *role < Role::Manager {
            continue;
        }
        match who {
            "others" if r.actor == Some(me.id) => continue,
            "shares" if !r.kind.sharing() => continue,
            _ => {}
        }
        let key = (r.kind, r.actor, r.via, node.parent_id);
        let gi = match open.get(&key) {
            Some(&gi) if (groups[gi].since - r.at).whole_seconds() <= WINDOW_SECS => gi,
            _ => {
                let parent_visible = node.parent_id.is_some_and(|p| roles.contains_key(&p));
                groups.push(Group {
                    kind: r.kind,
                    actor: None,
                    via: r.via,
                    mine: r.actor == Some(me.id),
                    at: r.at,
                    since: r.at,
                    count: 0,
                    items: Vec::new(),
                    folder: folders.get(&node.id).cloned().filter(|f| !f.is_empty()),
                    folder_id: if parent_visible { node.parent_id } else { None },
                    details: r.details.clone(),
                });
                let gi = groups.len() - 1;
                open.insert(key, gi);
                gi
            }
        };
        let g = &mut groups[gi];
        g.since = r.at;
        if seen.entry(gi).or_default().insert(node.id) {
            g.count += 1;
            if g.items.len() < ITEMS {
                g.items.push(Item {
                    node: NodeInfo::from(*node),
                    deleted: node.deleted_at.is_some(),
                    prev_name: r.prev_name,
                });
            }
        }
        if g.actor.is_none() {
            g.actor = r.actor.map(|id| Person {
                id,
                name: String::new(),
            });
        }
    }

    // Names of the people.
    let people: Vec<i64> = groups
        .iter()
        .filter_map(|g| g.actor.as_ref().map(|a| a.id))
        .collect();
    let names: HashMap<i64, String> =
        sqlx::query_as::<_, (i64, String)>("SELECT id, display_name FROM users WHERE id = ANY($1)")
            .bind(&people)
            .fetch_all(&st.db)
            .await?
            .into_iter()
            .collect();
    for g in &mut groups {
        if let Some(a) = &mut g.actor {
            a.name = names.get(&a.id).cloned().unwrap_or_default();
        }
    }
    Ok(Json(ActivityPage { groups, next }))
}

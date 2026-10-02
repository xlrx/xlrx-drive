//! Sync API (PLAN 5.2): the change feed of a root and live notifications.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::Stream;
use serde::{Deserialize, Serialize};
use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Rev};
use xlrx_sync::RemoteEntry;

use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::files::{live, roots};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct ChangesQuery {
    pub root: i64,
    /// Last cursor received; 0 (or none) for the first fetch.
    #[serde(default)]
    pub cursor: i64,
    pub limit: Option<i64>,
}

/// One node's current state; `None` if it was deleted (or is no longer visible).
#[derive(Serialize)]
pub struct Change {
    pub node: NodeId,
    pub state: Option<RemoteEntry>,
}

#[derive(Serialize)]
pub struct Changes {
    pub changes: Vec<Change>,
    pub cursor: i64,
    /// More changes follow: fetch them with `cursor` and apply all pages together. (A page may
    /// contain a node whose folder only comes on a later page.)
    pub more: bool,
}

fn entry(n: &NodeRow) -> Option<RemoteEntry> {
    let parent = n.parent_id?;
    let content = match (&n.content_hash, n.size) {
        (Some(h), Some(size)) => Some(FileContent {
            hash: ContentHash(h.as_slice().try_into().ok()?),
            size: size as u64,
        }),
        _ => None,
    };
    Some(RemoteEntry {
        parent: NodeId(parent as u64),
        name: Name::new(&n.name).ok()?,
        kind: if n.is_dir() { Kind::Dir } else { Kind::File },
        content,
        rev: Rev(n.rev as u64),
    })
}

/// Changes of a root since a cursor: the current state of every node changed since then, in
/// journal order. The first fetch (cursor 0) lists the live nodes only.
pub async fn changes(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<ChangesQuery>,
) -> ApiResult<Json<Changes>> {
    let root = db::root_by_id(&st.db, q.root)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !roots::can_read(&root, me.id) {
        return Err(ApiError::NotFound);
    }
    let limit = q.limit.unwrap_or(1000).clamp(1, 10_000);
    // One query, one snapshot: writers commit in sequence order (journal lock), so nothing with
    // a lower sequence can appear later.
    let mut rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes
         WHERE root_id = $1 AND seq > $2 AND parent_id IS NOT NULL
           AND ($2 > 0 OR deleted_at IS NULL)
         ORDER BY seq LIMIT $3"
    )))
    .bind(root.id)
    .bind(q.cursor)
    .bind(limit + 1)
    .fetch_all(&st.db)
    .await?;
    let more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    let cursor = rows.last().map_or(q.cursor, |n| n.seq);
    let changes = rows
        .iter()
        .map(|n| Change {
            node: NodeId(n.id as u64),
            state: if n.deleted_at.is_some() {
                None
            } else {
                entry(n)
            },
        })
        .collect();
    Ok(Json(Changes {
        changes,
        cursor,
        more,
    }))
}

#[derive(Serialize)]
struct Changed {
    root: i64,
    seq: i64,
}

/// Server-sent events: `change` with `[{"root": …, "seq": …}]` whenever something in a root the
/// person can see changed. Clients then fetch the changes (or reload the view).
pub async fn notify(
    State(st): State<AppState>,
    me: CurrentUser,
) -> ApiResult<Sse<impl Stream<Item = Result<Event, Infallible>>>> {
    let rx = live::subscribe(&st).await?;
    let last = *rx.borrow();
    let user = me.id;
    let stream =
        futures_util::stream::unfold((st, rx, last), move |(st, mut rx, mut last)| async move {
            loop {
                rx.changed().await.ok()?;
                let now = *rx.borrow_and_update();
                // Looked up each time: a root created (or shared) after connecting counts too.
                let visible: Vec<i64> = roots::readable(&st, user)
                    .await
                    .ok()?
                    .into_iter()
                    .map(|r| r.id)
                    .collect();
                let rows: Vec<(i64, i64)> = sqlx::query_as(
                    "SELECT root_id, max(seq) FROM journal
                     WHERE seq > $1 AND seq <= $2 AND root_id = ANY($3) GROUP BY root_id",
                )
                .bind(last)
                .bind(now)
                .bind(&visible)
                .fetch_all(&st.db)
                .await
                .ok()?;
                last = now;
                if rows.is_empty() {
                    continue;
                }
                let data: Vec<Changed> = rows
                    .into_iter()
                    .map(|(root, seq)| Changed { root, seq })
                    .collect();
                let event = Event::default()
                    .event("change")
                    .json_data(data)
                    .unwrap_or_default();
                return Some((Ok(event), (st, rx, last)));
            }
        });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

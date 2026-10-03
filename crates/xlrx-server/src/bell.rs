//! Notifications (PLAN 8.4): the bell in the web app, live through the event stream of
//! `/api/sync/notify`. A person only ever sees notifications about items they may still see.

use serde_json::{Value, json};
use sqlx::PgPool;

use crate::error::ApiResult;
use crate::files::access;
use crate::files::db::{NODE_COLS, NodeRow};
use crate::state::AppState;

/// Unread notifications listed (and counted) at most.
pub const LATEST: i64 = 100;

/// Tells the open event streams of these people to update their bell.
fn ring(st: &AppState, users: &[i64]) {
    for u in users {
        // No receiver (nobody connected) is fine.
        let _ = st.bell.send(*u);
    }
}

/// Something was shared with a person or a group (everyone in it but the one sharing).
pub async fn shared(
    st: &AppState,
    actor: i64,
    node_id: i64,
    user: Option<i64>,
    group: Option<i64>,
    details: Value,
) -> ApiResult<()> {
    let users: Vec<i64> = match (user, group) {
        (Some(u), _) => vec![u],
        (None, Some(g)) => {
            sqlx::query_scalar("SELECT user_id FROM group_members WHERE group_id = $1")
                .bind(g)
                .fetch_all(&st.db)
                .await?
        }
        (None, None) => Vec::new(),
    };
    let users: Vec<i64> = users.into_iter().filter(|u| *u != actor).collect();
    sqlx::query(
        "INSERT INTO notifications (user_id, kind, actor_user_id, node_id, details)
         SELECT u, 'shared', $2, $3, $4 FROM unnest($1::bigint[]) AS u",
    )
    .bind(&users)
    .bind(actor)
    .bind(node_id)
    .bind(details)
    .execute(&st.db)
    .await?;
    ring(st, &users);
    Ok(())
}

/// A file arrived through a link of `owner`'s: added to an unread notice of the same link from
/// the last hour, or a new one.
pub async fn link_upload(
    st: &AppState,
    owner: i64,
    link_id: i64,
    folder_id: i64,
    name: &str,
) -> ApiResult<()> {
    let merged = sqlx::query(
        "UPDATE notifications
            SET details = jsonb_set(
                    jsonb_set(details, '{count}', to_jsonb((details->>'count')::int + 1)),
                    '{names}',
                    CASE WHEN jsonb_array_length(details->'names') < 5
                         THEN (details->'names') || to_jsonb($3::text) ELSE details->'names' END),
                created_at = now()
          WHERE id = (SELECT id FROM notifications
                       WHERE user_id = $1 AND kind = 'link_upload' AND read_at IS NULL
                         AND (details->>'link')::bigint = $2 AND created_at > now() - interval '1 hour'
                       ORDER BY created_at DESC LIMIT 1)",
    )
    .bind(owner)
    .bind(link_id)
    .bind(name)
    .execute(&st.db)
    .await?;
    if merged.rows_affected() == 0 {
        sqlx::query(
            "INSERT INTO notifications (user_id, kind, node_id, details)
             VALUES ($1, 'link_upload', $2, $3)",
        )
        .bind(owner)
        .bind(folder_id)
        .bind(json!({ "link": link_id, "count": 1, "names": [name] }))
        .execute(&st.db)
        .await?;
    }
    ring(st, &[owner]);
    Ok(())
}

#[derive(sqlx::FromRow)]
pub struct Row {
    pub id: i64,
    pub kind: String,
    pub actor_user_id: Option<i64>,
    pub node_id: i64,
    pub details: Value,
    pub created_at: time::OffsetDateTime,
    pub read_at: Option<time::OffsetDateTime>,
}

/// The latest notifications whose items the person may still see, with those items.
pub async fn visible(db: &PgPool, user: i64) -> ApiResult<Vec<(Row, NodeRow)>> {
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, kind, actor_user_id, node_id, details, created_at, read_at
           FROM notifications WHERE user_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(user)
    .bind(LATEST)
    .fetch_all(db)
    .await?;
    let ids: Vec<i64> = rows.iter().map(|r| r.node_id).collect();
    let nodes: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE id = ANY($1) AND deleted_at IS NULL"
    )))
    .bind(&ids)
    .fetch_all(db)
    .await?;
    let roles = access::roles(db, user, &nodes).await?;
    let by_id: std::collections::HashMap<i64, NodeRow> = nodes
        .into_iter()
        .filter(|n| roles.contains_key(&n.id))
        .map(|n| (n.id, n))
        .collect();
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let n = by_id.get(&r.node_id)?.clone();
            Some((r, n))
        })
        .collect())
}

pub async fn unread(db: &PgPool, user: i64) -> ApiResult<usize> {
    Ok(visible(db, user)
        .await?
        .iter()
        .filter(|(r, _)| r.read_at.is_none())
        .count())
}

/// Marks the given (or all) notifications as read and updates the person's other windows.
pub async fn read(st: &AppState, user: i64, ids: Option<&[i64]>) -> ApiResult<()> {
    sqlx::query(
        "UPDATE notifications SET read_at = now()
          WHERE user_id = $1 AND read_at IS NULL AND ($2::bigint[] IS NULL OR id = ANY($2))",
    )
    .bind(user)
    .bind(ids)
    .execute(&st.db)
    .await?;
    ring(st, &[user]);
    Ok(())
}

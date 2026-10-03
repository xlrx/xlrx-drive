//! "Markiert" (PLAN 8.2): items a person starred, private to them, for the start page.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;

use super::files::{RecentItem, folder_names, visible};
use crate::auth::session::CurrentUser;
use crate::error::ApiResult;
use crate::files::access;
use crate::files::db::{NODE_COLS, NodeRow};
use crate::state::AppState;

/// Whether the person starred these nodes.
pub async fn starred(
    st: &AppState,
    user: i64,
    ids: &[i64],
) -> ApiResult<std::collections::HashSet<i64>> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT node_id FROM stars WHERE user_id = $1 AND node_id = ANY($2)",
    )
    .bind(user)
    .bind(ids)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .collect())
}

pub async fn star(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    visible(&st, &me, id).await?;
    sqlx::query("INSERT INTO stars (user_id, node_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(me.id)
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Removing a star needs no access: it only ever touches the person's own list.
pub async fn unstar(
    State(st): State<AppState>,
    me: CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM stars WHERE user_id = $1 AND node_id = $2")
        .bind(me.id)
        .bind(id)
        .execute(&st.db)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// The person's starred items that are still there and may still be seen, newest star first.
pub async fn list(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<Vec<RecentItem>>> {
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM stars s JOIN nodes n ON n.id = s.node_id
          WHERE s.user_id = $1 AND n.deleted_at IS NULL
          ORDER BY s.created_at DESC LIMIT 200",
        NODE_COLS
            .split(',')
            .map(|c| format!("n.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    )))
    .bind(me.id)
    .fetch_all(&st.db)
    .await?;
    let roles = access::roles(&st.db, me.id, &rows).await?;
    let rows: Vec<NodeRow> = rows
        .into_iter()
        .filter(|n| roles.contains_key(&n.id))
        .collect();
    let scope = access::scope(&st.db, me.id).await?;
    let folders = folder_names(&st, &scope, &rows).await?;
    Ok(Json(
        rows.iter()
            .map(|n| RecentItem {
                node: n.into(),
                folder: folders.get(&n.id).cloned().unwrap_or_default(),
            })
            .collect(),
    ))
}

//! Events the journal does not know (PLAN 8.1): sharing and public links, for the activity
//! stream. Changes to files themselves are read from the journal.

use sqlx::PgPool;

use crate::error::ApiResult;

/// `kind`: `shared`, `link_created`, `link_download`, `link_upload`, `link_edit`. `actor`: `None`
/// for someone using a public link.
pub async fn record(
    db: &PgPool,
    actor: Option<i64>,
    node_id: i64,
    kind: &str,
    details: serde_json::Value,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO events (actor_user_id, node_id, kind, details) VALUES ($1, $2, $3, $4)",
    )
    .bind(actor)
    .bind(node_id)
    .bind(kind)
    .bind(details)
    .execute(db)
    .await?;
    Ok(())
}

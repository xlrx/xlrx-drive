//! Audit-Log: Anmeldungen, Änderungen an Faktoren, Admin-Aktionen (PLAN 16).

use sqlx::PgPool;

use crate::error::ApiResult;

pub async fn log(
    db: &PgPool,
    actor: Option<i64>,
    target: Option<i64>,
    action: &str,
    ip: Option<&str>,
    details: serde_json::Value,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO audit_log (actor_user_id, target_user_id, action, ip, details)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(actor)
    .bind(target)
    .bind(action)
    .bind(ip)
    .bind(details)
    .execute(db)
    .await?;
    tracing::info!(action, ?actor, ?target, ip, "audit");
    Ok(())
}

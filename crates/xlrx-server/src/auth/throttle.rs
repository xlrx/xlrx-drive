//! Brute-force protection: count failed attempts per key, lock out exponentially past a threshold.
//!
//! Keys are the IP ("ip:…") and the entered username ("name:…"), including for accounts that do not
//! exist. That way an unknown account behaves exactly like a known one.

use crate::error::{ApiError, ApiResult};
use sqlx::PgPool;

/// Window in which failed attempts are added up.
const WINDOW_MINUTES: i64 = 15;
/// Lock out after this many failed attempts (per username or per IP; a family IP is shared by
/// several people).
const NAME_LIMIT: i32 = 5;
const IP_LIMIT: i32 = 20;
const MAX_LOCK_SECS: i64 = 3600;

pub fn ip_key(ip: &str) -> String {
    format!("ip:{ip}")
}

pub fn name_key(folded: &str) -> String {
    format!("name:{folded}")
}

fn limit(key: &str) -> i32 {
    if key.starts_with("ip:") {
        IP_LIMIT
    } else {
        NAME_LIMIT
    }
}

/// Rejects as long as one of the keys is locked.
pub async fn check(db: &PgPool, keys: &[String]) -> ApiResult<()> {
    let locked: Option<(String,)> = sqlx::query_as(
        "SELECT key FROM auth_throttle WHERE key = ANY($1) AND locked_until > now() LIMIT 1",
    )
    .bind(keys)
    .fetch_optional(db)
    .await?;
    match locked {
        Some(_) => Err(ApiError::TooManyRequests),
        None => Ok(()),
    }
}

/// Counts a failed attempt. Returns `true` if this caused a lockout.
pub async fn fail(db: &PgPool, keys: &[String]) -> ApiResult<bool> {
    let mut locked_now = false;
    for key in keys {
        let (failures,): (i32,) = sqlx::query_as(
            "INSERT INTO auth_throttle (key, failures, first_failure_at) VALUES ($1, 1, now())
             ON CONFLICT (key) DO UPDATE SET
               failures = CASE WHEN auth_throttle.first_failure_at < now() - make_interval(mins => $2)
                               THEN 1 ELSE auth_throttle.failures + 1 END,
               first_failure_at = CASE WHEN auth_throttle.first_failure_at < now() - make_interval(mins => $2)
                               THEN now() ELSE auth_throttle.first_failure_at END
             RETURNING failures",
        )
        .bind(key)
        .bind(WINDOW_MINUTES as i32)
        .fetch_one(db)
        .await?;
        let lim = limit(key);
        if failures >= lim {
            let exp = (failures - lim).min(10) as u32;
            let secs = (30i64 << exp).min(MAX_LOCK_SECS);
            sqlx::query(
                "UPDATE auth_throttle SET locked_until = now() + make_interval(secs => $2) WHERE key = $1",
            )
            .bind(key)
            .bind(secs as f64)
            .execute(db)
            .await?;
            locked_now = true;
        }
    }
    Ok(locked_now)
}

pub async fn reset(db: &PgPool, key: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM auth_throttle WHERE key = $1")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

//! Schutz vor Durchprobieren: Fehlversuche je Schlüssel zählen, ab einer Schwelle exponentiell sperren.
//!
//! Schlüssel sind die IP („ip:…“) und der eingegebene Benutzername („name:…“), auch für Konten, die es
//! nicht gibt. So verhält sich ein unbekanntes Konto genauso wie ein bekanntes.

use crate::error::{ApiError, ApiResult};
use sqlx::PgPool;

/// Fenster, in dem Fehlversuche zusammengezählt werden.
const WINDOW_MINUTES: i64 = 15;
/// Ab so vielen Fehlversuchen wird gesperrt (pro Benutzername bzw. pro IP; hinter einer
/// Familien-IP liegen mehrere Personen).
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

/// Lehnt ab, solange einer der Schlüssel gesperrt ist.
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

/// Zählt einen Fehlversuch. Liefert `true`, wenn dadurch eine Sperre entstanden ist.
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

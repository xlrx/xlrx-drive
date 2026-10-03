//! Job queue in Postgres (PLAN 6.6): leased with `FOR UPDATE SKIP LOCKED`, retried with backoff,
//! kept as dead letter after the last attempt.

use std::time::Duration;

use serde::Serialize;
use sqlx::PgPool;

/// A worker owns a job this long (longer than the slowest job: OCR of a long scan); if it
/// crashes, the job comes back afterwards.
const LEASE: Duration = Duration::from_secs(3 * 3600);
/// Attempts before a job is given up (it stays as 'failed').
pub const MAX_ATTEMPTS: i32 = 5;

#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub key: String,
    pub attempts: i32,
}

/// Adds jobs unless one for the same thing exists already (queued, waiting or failed).
pub async fn enqueue(db: &PgPool, kind: &str, jobs: &[(String, i16)]) -> Result<u64, sqlx::Error> {
    if jobs.is_empty() {
        return Ok(0);
    }
    let (keys, priorities): (Vec<&str>, Vec<i16>) =
        jobs.iter().map(|(k, p)| (k.as_str(), *p)).unzip();
    let r = sqlx::query(
        "INSERT INTO jobs (kind, key, priority)
         SELECT $1, k, p FROM unnest($2::text[], $3::smallint[]) AS t(k, p)
         ON CONFLICT (kind, key) DO NOTHING",
    )
    .bind(kind)
    .bind(&keys)
    .bind(&priorities)
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Leases the most urgent due job of one of the given kinds.
pub async fn claim(db: &PgPool, kinds: &[&str]) -> Result<Option<Job>, sqlx::Error> {
    sqlx::query_as(
        "UPDATE jobs SET run_after = now() + make_interval(secs => $2), attempts = attempts + 1
          WHERE id = (SELECT id FROM jobs
                       WHERE state = 'queued' AND run_after <= now() AND kind = ANY($1)
                       ORDER BY priority DESC, run_after, id
                       LIMIT 1 FOR UPDATE SKIP LOCKED)
          RETURNING id, kind, key, attempts",
    )
    .bind(kinds)
    .bind(LEASE.as_secs_f64())
    .fetch_optional(db)
    .await
}

/// The job is done (or has nothing left to do).
pub async fn finish(db: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM jobs WHERE id = $1")
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

/// Wait before the next attempt: 1 min, 10 min, 1 h, 6 h.
fn backoff(attempts: i32) -> Duration {
    Duration::from_secs(match attempts {
        ..=1 => 60,
        2 => 600,
        3 => 3600,
        _ => 6 * 3600,
    })
}

/// The attempt failed: try again later, or give up after the last attempt.
pub async fn retry(db: &PgPool, job: &Job, error: &str) -> Result<(), sqlx::Error> {
    let give_up = job.attempts >= MAX_ATTEMPTS;
    sqlx::query(
        "UPDATE jobs SET state = $2, run_after = now() + make_interval(secs => $3), last_error = $4
          WHERE id = $1",
    )
    .bind(job.id)
    .bind(if give_up { "failed" } else { "queued" })
    .bind(backoff(job.attempts).as_secs_f64())
    .bind(error)
    .execute(db)
    .await?;
    Ok(())
}

/// The job could not start (its worker pauses): back in the queue, the attempt not counted.
pub async fn release(db: &PgPool, id: i64) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE jobs SET run_after = now(), attempts = greatest(attempts - 1, 0) WHERE id = $1",
    )
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

/// Gives a job up at once: trying again cannot help (it stays as 'failed').
pub async fn fail(db: &PgPool, id: i64, error: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE jobs SET state = 'failed', last_error = $2 WHERE id = $1")
        .bind(id)
        .bind(error)
        .execute(db)
        .await?;
    Ok(())
}

/// The job needs a tool that is not set up: it waits until the server starts with it.
pub async fn park(db: &PgPool, job: &Job, reason: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE jobs SET state = 'waiting', attempts = 0, last_error = $2 WHERE id = $1")
        .bind(job.id)
        .bind(reason)
        .execute(db)
        .await?;
    Ok(())
}

/// Lets waiting jobs of a kind run again (a tool is available now).
pub async fn resume_waiting(db: &PgPool, kind: &str) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'queued', run_after = now(), last_error = NULL
          WHERE state = 'waiting' AND kind = $1",
    )
    .bind(kind)
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

/// Gives failed jobs another round of attempts (admin).
pub async fn retry_failed(db: &PgPool) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'queued', attempts = 0, run_after = now() WHERE state = 'failed'",
    )
    .execute(db)
    .await?;
    Ok(r.rows_affected())
}

#[derive(Serialize, Default, Debug, PartialEq)]
pub struct Counts {
    pub queued: i64,
    pub waiting: i64,
    pub failed: i64,
}

pub async fn counts(db: &PgPool) -> Result<Counts, sqlx::Error> {
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT state, count(*) FROM jobs GROUP BY state")
            .fetch_all(db)
            .await?;
    let mut c = Counts::default();
    for (state, n) in rows {
        match state.as_str() {
            "queued" => c.queued = n,
            "waiting" => c.waiting = n,
            "failed" => c.failed = n,
            _ => {}
        }
    }
    Ok(c)
}

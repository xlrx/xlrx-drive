//! The xlrx-drive server.
//!
//! As of M0: accounts and sign-in (password + TOTP or passkey, recovery codes, sessions, step-up),
//! account management, audit log. Sync, storage and search follow from M1 (see `docs/PLAN.md`).

pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod files;
pub mod state;
pub mod users;
pub mod web;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

pub use api::router;
pub use config::Config;
pub use state::AppState;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn connect(database_url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(16)
        .connect(database_url)
        .await
}

/// Periodically cleans up expired sessions, intermediate steps, lockouts and device tokens.
pub async fn cleanup(db: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM sessions WHERE expires_at < now() OR last_seen_at < now() - interval '8 hours'")
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM login_challenges WHERE expires_at < now()")
        .execute(db)
        .await?;
    sqlx::query("DELETE FROM invites WHERE expires_at < now() - interval '30 days'")
        .execute(db)
        .await?;
    sqlx::query(
        "DELETE FROM auth_throttle WHERE first_failure_at < now() - interval '1 day'
           AND (locked_until IS NULL OR locked_until < now())",
    )
    .execute(db)
    .await?;
    auth::device::cleanup(db).await?;
    Ok(())
}

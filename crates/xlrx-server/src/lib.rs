//! Server von xlrx-drive.
//!
//! Stand M0: Konten und Anmeldung (Passwort + TOTP oder Passkey, Wiederherstellungscodes, Sitzungen,
//! Step-up), Verwaltung von Konten, Audit-Log. Sync, Speicher und Suche folgen ab M1 (siehe `docs/PLAN.md`).

pub mod api;
pub mod audit;
pub mod auth;
pub mod config;
pub mod error;
pub mod state;
pub mod users;

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

/// Räumt periodisch abgelaufene Sitzungen, Zwischenschritte und Sperren auf.
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
    Ok(())
}

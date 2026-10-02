//! Gemeinsamer Zustand aller Anfragen.

use std::ops::Deref;
use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::Semaphore;
use webauthn_rs::{Webauthn, WebauthnBuilder};

use crate::auth::ceremony::Ceremonies;
use crate::auth::password::Passwords;
use crate::auth::secret::SecretBox;
use crate::config::Config;
use crate::error::{ApiError, ApiResult};

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub db: PgPool,
    pub cfg: Config,
    pub secrets: SecretBox,
    pub passwords: Passwords,
    pub webauthn: Webauthn,
    pub ceremonies: Ceremonies,
    /// Begrenzt gleichzeitige Passwort-Prüfungen (argon2 braucht CPU und RAM; Schutz vor Überlast).
    pub hashing: Semaphore,
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub fn new(db: PgPool, cfg: Config) -> Result<Self, String> {
        let passwords = Passwords::new(cfg.argon2, cfg.password_blocklist.as_deref())?;
        // Nur die konfigurierten Adressen dürfen Passkeys verwenden – nicht jede Subdomain der
        // Passkey-Domain (sonst könnte eine Lücke in einem anderen Dienst unter *.domain Anmeldungen
        // bei xlrx auslösen).
        let mut builder = WebauthnBuilder::new(&cfg.rp_id, &cfg.public_url)
            .map_err(|e| format!("Passkey-Konfiguration: {e}"))?
            .rp_name("xlrx-drive")
            .allow_subdomains(false);
        for o in &cfg.extra_origins {
            builder = builder.append_allowed_origin(o);
        }
        let webauthn = builder
            .build()
            .map_err(|e| format!("Passkey-Konfiguration: {e}"))?;
        Ok(Self(Arc::new(Inner {
            db,
            secrets: SecretBox::new(&cfg.secret_key),
            cfg,
            passwords,
            webauthn,
            ceremonies: Ceremonies::default(),
            hashing: Semaphore::new(2),
        })))
    }

    /// Passwort hashen (blockierend, begrenzt parallel).
    pub async fn hash_password(&self, password: String) -> ApiResult<String> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.passwords.hash(&password))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?
            .map_err(ApiError::Internal)
    }

    pub async fn verify_password(
        &self,
        stored: Option<String>,
        password: String,
    ) -> ApiResult<bool> {
        let _permit = self
            .hashing
            .acquire()
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))?;
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.passwords.verify(stored.as_deref(), &password))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))
    }
}

//! State shared by all requests.

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

/// An async lock shared by everyone working on the same thing.
type Lock = Arc<tokio::sync::Mutex<()>>;

pub struct Inner {
    pub db: PgPool,
    pub cfg: Config,
    pub secrets: SecretBox,
    pub passwords: Passwords,
    pub webauthn: Webauthn,
    pub ceremonies: Ceremonies,
    /// Limits concurrent password checks (argon2 needs CPU and RAM; protection against overload).
    pub hashing: Semaphore,
    /// Limits concurrent thumbnail rendering (decoding photos is heavy for the NAS CPU).
    pub thumbnails: Semaphore,
    /// Content Security Policy for the web app (inline script hashes), if one is configured.
    pub web_csp: Option<String>,
    /// One lock per root: scans and changes through the API never run at the same time on the
    /// same directory tree.
    root_locks: std::sync::Mutex<std::collections::HashMap<i64, Arc<tokio::sync::Mutex<()>>>>,
    /// Highest committed journal sequence, live (see [`crate::files::live`]).
    pub live: tokio::sync::OnceCell<tokio::sync::watch::Receiver<i64>>,
    /// One lock per (person, device): a device's sync operations run one after another, so a
    /// retried operation never runs twice at the same time.
    sync_locks: std::sync::Mutex<std::collections::HashMap<(i64, String), Lock>>,
    /// Full-text search, once started (see [`crate::search::start`]).
    pub search: std::sync::OnceLock<crate::search::Search>,
    /// Running watchers per root (dropping one stops it).
    watchers: std::sync::Mutex<std::collections::HashMap<i64, notify::RecommendedWatcher>>,
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
        // Only the configured addresses may use passkeys – not every subdomain of the passkey
        // domain (otherwise a vulnerability in another service under *.domain could trigger
        // sign-ins at xlrx).
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
        let web_csp = cfg
            .web_dir
            .as_deref()
            .map(crate::web::csp_for_dir)
            .transpose()?;
        Ok(Self(Arc::new(Inner {
            web_csp,
            root_locks: Default::default(),
            watchers: Default::default(),
            sync_locks: Default::default(),
            live: Default::default(),
            search: Default::default(),
            db,
            secrets: SecretBox::new(&cfg.secret_key),
            cfg,
            passwords,
            webauthn,
            ceremonies: Ceremonies::default(),
            hashing: Semaphore::new(2),
            thumbnails: Semaphore::new(2),
        })))
    }

    /// The lock of a root (see [`Inner::root_locks`]).
    pub fn root_lock(&self, root_id: i64) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.root_locks.lock().expect("mutex");
        map.entry(root_id).or_default().clone()
    }

    /// The sync lock of a device (see [`Inner::sync_locks`]).
    pub fn sync_lock(&self, user_id: i64, device: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.sync_locks.lock().expect("mutex");
        map.entry((user_id, device.to_owned())).or_default().clone()
    }

    /// Is the root watched already?
    pub fn watching(&self, root_id: i64) -> bool {
        self.watchers.lock().expect("mutex").contains_key(&root_id)
    }

    /// Keeps a root's watcher running. False if another one was registered meanwhile (this one is
    /// dropped and stops).
    pub fn keep_watcher(&self, root_id: i64, w: notify::RecommendedWatcher) -> bool {
        let mut map = self.watchers.lock().expect("mutex");
        if map.contains_key(&root_id) {
            return false;
        }
        map.insert(root_id, w);
        true
    }

    /// Hash a password (blocking, with limited parallelism).
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

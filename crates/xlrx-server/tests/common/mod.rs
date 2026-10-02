//! Test environment: a separate database per test, router without networking, client with cookie
//! and origin.
//!
//! Requires a PostgreSQL server:
//! `XLRX_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres`.
//! Without the variable, the tests are skipped (with a notice); CI always sets it.

#![allow(dead_code)]

pub mod files;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sqlx::{Connection, Executor, PgConnection, PgPool};
use tower::ServiceExt;
use xlrx_server::config::{ArgonParams, Config};
use xlrx_server::{AppState, users};

pub const ORIGIN: &str = "https://drive.example.test";

pub struct TestDb {
    pub pool: PgPool,
    name: String,
    admin_url: String,
}

impl TestDb {
    pub async fn new() -> Option<Self> {
        let Ok(admin_url) = std::env::var("XLRX_TEST_DATABASE_URL") else {
            // In CI, skipping is an error: there, tests must run against a real PostgreSQL.
            assert!(
                std::env::var_os("CI").is_none(),
                "XLRX_TEST_DATABASE_URL fehlt in der CI"
            );
            eprintln!("XLRX_TEST_DATABASE_URL nicht gesetzt – Datenbank-Test übersprungen");
            return None;
        };
        let name = format!(
            "xlrx_test_{}",
            uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .get(..16)
                .unwrap_or("x")
        );
        let mut admin = PgConnection::connect(&admin_url)
            .await
            .expect("Verbindung zum Testserver");
        admin
            .execute(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .await
            .expect("Testdatenbank anlegen");
        let mut url: url::Url = admin_url.parse().expect("URL");
        url.set_path(&name);
        let pool = xlrx_server::connect(url.as_str()).await.expect("Pool");
        xlrx_server::MIGRATOR.run(&pool).await.expect("Migrationen");
        Some(Self {
            pool,
            name,
            admin_url,
        })
    }

    pub async fn drop_db(self) {
        self.pool.close().await;
        if let Ok(mut admin) = PgConnection::connect(&self.admin_url).await {
            let _ = admin
                .execute(sqlx::AssertSqlSafe(format!(
                    "DROP DATABASE IF EXISTS {} WITH (FORCE)",
                    self.name
                )))
                .await;
        }
    }
}

pub fn config() -> Config {
    Config {
        database_url: String::new(),
        bind: "127.0.0.1:0".parse().unwrap(),
        public_url: ORIGIN.parse().unwrap(),
        extra_origins: vec![],
        rp_id: "drive.example.test".into(),
        secret_key: [42u8; 32],
        web_dir: None,
        trust_proxy: false,
        // Fast parameters, for tests only.
        argon2: ArgonParams {
            m_kib: 256,
            t: 1,
            p: 1,
        },
        password_blocklist: None,
        data_dir: None,
        state_dir: None,
        home_pattern: "homes/{user}/Drive".into(),
        force_copy: false,
    }
}

pub struct Env {
    pub db: TestDb,
    pub state: AppState,
    pub app: Router,
    /// Data directory (only with [`Env::with_data`]); removed when dropped.
    pub data: Option<tempfile::TempDir>,
}

impl Env {
    pub async fn new() -> Option<Self> {
        Self::build(None).await
    }

    /// Like [`Env::new`], with a temporary data directory for roots and files.
    pub async fn with_data() -> Option<Self> {
        Self::build(Some(tempfile::tempdir().expect("Datenverzeichnis"))).await
    }

    /// Like [`Env::with_data`], but moving between roots and the state directory always copies
    /// (as between Btrfs subvolumes on the NAS).
    pub async fn with_data_copying() -> Option<Self> {
        let mut env = Self::with_data().await?;
        let mut cfg = env.state.cfg.clone();
        cfg.force_copy = true;
        env.state = AppState::new(env.db.pool.clone(), cfg).expect("Zustand");
        env.app = xlrx_server::router(env.state.clone());
        Some(env)
    }

    async fn build(data: Option<tempfile::TempDir>) -> Option<Self> {
        let db = TestDb::new().await?;
        let mut cfg = config();
        cfg.data_dir = data.as_ref().map(|d| d.path().to_path_buf());
        cfg.state_dir = data.as_ref().map(|d| d.path().join("xlrx-state"));
        let state = AppState::new(db.pool.clone(), cfg).expect("Zustand");
        let app = xlrx_server::router(state.clone());
        Some(Self {
            db,
            state,
            app,
            data,
        })
    }

    pub fn data_dir(&self) -> &std::path::Path {
        self.data.as_ref().expect("Env::with_data").path()
    }

    pub fn client(&self) -> Client {
        Client {
            app: self.app.clone(),
            cookie: None,
            origin: Some(ORIGIN.into()),
        }
    }

    /// Create an account with a setup link (like `xlrx-server create-user`).
    pub async fn invite(&self, username: &str, admin: bool) -> String {
        let u = users::create(
            &self.db.pool,
            username,
            &format!("{username} Test"),
            None,
            admin,
        )
        .await
        .expect("Konto");
        users::create_invite(&self.db.pool, u.id, None)
            .await
            .expect("Einladung")
    }

    pub async fn finish(self) {
        self.db.drop_db().await;
    }
}

pub struct Client {
    app: Router,
    pub cookie: Option<String>,
    pub origin: Option<String>,
}

pub struct Resp {
    pub status: StatusCode,
    pub body: Value,
}

/// A response with headers and the raw body (downloads).
pub struct RawResp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub bytes: Vec<u8>,
}

impl RawResp {
    pub fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

impl Resp {
    pub fn ok(&self) -> &Value {
        assert!(
            self.status.is_success(),
            "Status {}: {}",
            self.status,
            self.body
        );
        &self.body
    }

    pub fn err(&self) -> String {
        self.body["error"].as_str().unwrap_or_default().to_owned()
    }
}

impl Client {
    pub async fn send(&mut self, method: &str, path: &str, body: Option<Value>) -> Resp {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(c) = &self.cookie {
            req = req.header(header::COOKIE, format!("xlrx_session={c}"));
        }
        if let Some(o) = &self.origin {
            req = req.header(header::ORIGIN, o);
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        for v in res.headers().get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            if let Some(rest) = v.strip_prefix("xlrx_session=") {
                let token = rest.split(';').next().unwrap_or_default();
                assert!(
                    v.contains("HttpOnly") && v.contains("SameSite=Strict") && v.contains("Secure")
                );
                self.cookie = (!token.is_empty()).then(|| token.to_owned());
            }
        }
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&bytes) }));
        Resp { status, body }
    }

    pub async fn post(&mut self, path: &str, body: Value) -> Resp {
        self.send("POST", path, Some(body)).await
    }

    pub async fn get(&mut self, path: &str) -> Resp {
        self.send("GET", path, None).await
    }

    /// A request with a raw body (uploads).
    pub async fn send_bytes(&mut self, method: &str, path: &str, body: Vec<u8>) -> Resp {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, "application/octet-stream");
        if let Some(c) = &self.cookie {
            req = req.header(header::COOKIE, format!("xlrx_session={c}"));
        }
        if let Some(o) = &self.origin {
            req = req.header(header::ORIGIN, o);
        }
        let res = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&bytes) }));
        Resp { status, body }
    }

    /// GET with extra headers, returning headers and the raw body.
    pub async fn get_raw(&mut self, path: &str, headers: &[(&str, &str)]) -> RawResp {
        let mut req = Request::builder().method("GET").uri(path);
        if let Some(c) = &self.cookie {
            req = req.header(header::COOKIE, format!("xlrx_session={c}"));
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(req.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        RawResp {
            status,
            headers,
            bytes,
        }
    }
}

/// Base32 → bytes (for the TOTP secret from the setup).
pub fn base32_decode(s: &str) -> Vec<u8> {
    const A: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let (mut buf, mut bits, mut out) = (0u32, 0u32, Vec::new());
    for c in s.chars() {
        buf = (buf << 5) | A.find(c).expect("Base32") as u32;
        bits += 5;
        if bits >= 8 {
            out.push((buf >> (bits - 8)) as u8);
            bits -= 8;
        }
    }
    out
}

pub fn now_step() -> u64 {
    xlrx_server::auth::totp::current_step(users::unix_now())
}

pub const PASSWORD: &str = "Wolken über dem Garten 7";

/// Setup with password + TOTP. Returns the client (signed in), the TOTP secret and the recovery
/// codes.
pub async fn setup_with_totp(env: &Env, invite: &str) -> (Client, Vec<u8>, Vec<String>) {
    let mut c = env.client();
    let start = c
        .post("/api/setup/start", json!({ "invite": invite }))
        .await;
    let token = start.ok()["setup_token"].as_str().unwrap().to_owned();
    c.post(
        "/api/setup/password",
        json!({ "setup_token": token, "password": PASSWORD }),
    )
    .await
    .ok();
    let begin = c
        .post("/api/setup/totp/begin", json!({ "setup_token": token }))
        .await;
    let secret = base32_decode(begin.ok()["secret"].as_str().unwrap());
    let code = xlrx_server::auth::totp::code_at_step(&secret, now_step());
    let done = c
        .post(
            "/api/setup/totp/confirm",
            json!({ "setup_token": token, "code": code }),
        )
        .await;
    let codes: Vec<String> = done.ok()["recovery_codes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert!(c.cookie.is_some(), "nach der Einrichtung angemeldet");
    (c, secret, codes)
}

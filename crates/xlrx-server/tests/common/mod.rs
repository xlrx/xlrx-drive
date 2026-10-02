//! Testumgebung: eigene Datenbank je Test, Router ohne Netzwerk, Client mit Cookie und Origin.
//!
//! Braucht einen PostgreSQL-Server: `XLRX_TEST_DATABASE_URL=postgres://postgres@127.0.0.1:5432/postgres`.
//! Ohne die Variable werden die Tests übersprungen (mit Hinweis); die CI setzt sie immer.

#![allow(dead_code)]

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
            // In der CI ist Überspringen ein Fehler: Dort muss gegen echtes PostgreSQL getestet werden.
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
        // Schnelle Parameter nur für Tests.
        argon2: ArgonParams {
            m_kib: 256,
            t: 1,
            p: 1,
        },
        password_blocklist: None,
    }
}

pub struct Env {
    pub db: TestDb,
    pub state: AppState,
    pub app: Router,
}

impl Env {
    pub async fn new() -> Option<Self> {
        let db = TestDb::new().await?;
        let state = AppState::new(db.pool.clone(), config()).expect("Zustand");
        let app = xlrx_server::router(state.clone());
        Some(Self { db, state, app })
    }

    pub fn client(&self) -> Client {
        Client {
            app: self.app.clone(),
            cookie: None,
            origin: Some(ORIGIN.into()),
        }
    }

    /// Konto mit Einrichtungslink anlegen (wie `xlrx-server create-user`).
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
}

/// Base32 → Bytes (für das TOTP-Geheimnis aus der Einrichtung).
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

/// Einrichtung mit Passwort + TOTP. Liefert Client (angemeldet), TOTP-Geheimnis und Wiederherstellungscodes.
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

//! Text extraction: PDFs, scans (OCR), text files and Office documents (Tika) become searchable;
//! jobs wait for missing programs; a text never lands under the wrong content.
//!
//! Needs `pdftotext`, `pdftoppm` and `tesseract` with German (Debian: poppler-utils,
//! tesseract-ocr-deu). Without them the tests are skipped locally and fail in CI.

mod common;

use std::time::Duration;

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::put;
use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::config::Config;
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;
use xlrx_server::{AppState, extract, search};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIXTURES}/{name}")).unwrap()
}

/// Are the programs there? In CI they must be.
fn tools_present() -> bool {
    let ok = ["pdftotext", "pdftoppm", "tesseract"].iter().all(|t| {
        std::process::Command::new(t)
            .arg(if *t == "tesseract" { "--version" } else { "-v" })
            .output()
            .is_ok_and(|o| o.status.success())
    });
    if !ok {
        assert!(
            std::env::var_os("CI").is_none(),
            "pdftotext/pdftoppm/tesseract fehlen in der CI"
        );
        eprintln!("pdftotext/pdftoppm/tesseract fehlen – Extraktionstest übersprungen");
    }
    ok
}

/// The environment with changed settings (before anything is started).
fn configure(env: &mut Env, change: impl FnOnce(&mut Config)) {
    let mut cfg = env.state.cfg.clone();
    change(&mut cfg);
    env.state = AppState::new(env.db.pool.clone(), cfg).unwrap();
    env.app = xlrx_server::router(env.state.clone());
}

async fn start(env: &Env) {
    search::start(&env.state).await.unwrap();
    extract::start(&env.state).await.unwrap();
}

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

async fn names(c: &mut Client, q: &str) -> Vec<String> {
    let q: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    let r = c.get(&format!("/api/search?q={q}")).await.ok().clone();
    let mut v: Vec<String> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

/// Waits until a search finds something (extraction and OCR take a moment).
async fn eventually(c: &mut Client, q: &str) -> Vec<String> {
    for _ in 0..600 {
        let found = names(c, q).await;
        if !found.is_empty() {
            return found;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("„{q}“ nicht gefunden");
}

async fn jobs(env: &Env) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as("SELECT key, state, last_error FROM jobs WHERE kind = 'extract' ORDER BY id")
        .fetch_all(&env.db.pool)
        .await
        .unwrap()
}

async fn wait_for_jobs(env: &Env, done: impl Fn(&[(String, String, Option<String>)]) -> bool) {
    for _ in 0..600 {
        if done(&jobs(env).await) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("Aufträge nicht erledigt: {:?}", jobs(env).await);
}

async fn sources(env: &Env) -> Vec<String> {
    sqlx::query_scalar("SELECT source FROM content_text ORDER BY source")
        .fetch_all(&env.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn pdf_scan_und_textdatei_werden_durchsuchbar() {
    if !tools_present() {
        return;
    }
    let Some(env) = Env::with_data().await else {
        return;
    };
    start(&env).await;
    let (mut c, root_id, _, dir) = signed_in(&env, "anna").await;
    write(&dir.join("Belege/Beleg 1.pdf"), &fixture("rechnung.pdf"));
    write(&dir.join("Dokument 2.pdf"), &fixture("scan.pdf"));
    write(&dir.join("Scan 3.tif"), &fixture("scan.tif"));
    // Latin-1, as older Windows programs save.
    write(&dir.join("notiz.txt"), b"Gr\xFC\xDFe vom Bodensee");
    // Same content twice: read once.
    write(&dir.join("Kopie.pdf"), &fixture("rechnung.pdf"));
    write(&dir.join("Urlaub.jpg"), b"\xFF\xD8\xFF");
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();

    assert_eq!(
        eventually(&mut c, "Wärmepumpe").await,
        ["Beleg 1.pdf", "Kopie.pdf"]
    );
    assert_eq!(eventually(&mut c, "Kaution").await, ["Dokument 2.pdf"]);
    assert_eq!(eventually(&mut c, "Kontoauszug").await, ["Scan 3.tif"]);
    assert_eq!(eventually(&mut c, "Grüße").await, ["notiz.txt"]);
    // German word stems in recognised text too.
    assert_eq!(names(&mut c, "Mietverträge").await, ["Dokument 2.pdf"]);
    let snippet = {
        let r = c.get("/api/search?q=Heizungswartung").await.ok().clone();
        r["hits"][0]["snippet"].clone()
    };
    assert!(
        snippet
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["hit"] == true && p["text"] == "Heizungswartung"),
        "{snippet}"
    );
    wait_for_jobs(&env, |j| j.is_empty()).await;
    assert_eq!(sources(&env).await, ["ocr", "ocr", "pdf", "plain"]);
    env.finish().await;
}

#[tokio::test]
async fn auftraege_warten_auf_fehlende_programme() {
    if !tools_present() {
        return;
    }
    let Some(mut env) = Env::with_data().await else {
        return;
    };
    configure(&mut env, |cfg| cfg.tesseract = "/gibt/es/nicht".into());
    start(&env).await;
    let (mut c, root_id, _, dir) = signed_in(&env, "bert").await;
    write(&dir.join("Dokument.pdf"), &fixture("scan.pdf"));
    write(&dir.join("Protokoll.docx"), b"PK\x03\x04 kein echtes Word");
    write(&dir.join("Beleg.pdf"), &fixture("rechnung.pdf"));
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();
    // What can be read is read; the rest waits and says why.
    assert_eq!(eventually(&mut c, "Wärmepumpe").await, ["Beleg.pdf"]);
    wait_for_jobs(&env, |j| {
        j.len() == 2 && j.iter().all(|(_, s, _)| s == "waiting")
    })
    .await;
    let mut reasons: Vec<String> = jobs(&env)
        .await
        .into_iter()
        .map(|(_, _, e)| e.unwrap())
        .collect();
    reasons.sort();
    assert_eq!(
        reasons,
        [
            "Texterkennung (OCR) nicht eingerichtet",
            "Tika nicht eingerichtet"
        ]
    );
    assert!(names(&mut c, "Kaution").await.is_empty());

    // The server starts again with the programs: waiting jobs run.
    let tika = mock_tika().await;
    let cookie = c.cookie.clone();
    env.state.search.get().unwrap().stop().await;
    env.state.extract.get().unwrap().stop().await;
    configure(&mut env, |cfg| {
        cfg.tesseract = "tesseract".into();
        cfg.tika_url = Some(tika);
    });
    start(&env).await;
    let mut c = env.client();
    c.cookie = cookie;
    assert_eq!(eventually(&mut c, "Kaution").await, ["Dokument.pdf"]);
    assert_eq!(
        eventually(&mut c, "Eigentümerversammlung").await,
        ["Protokoll.docx"]
    );
    wait_for_jobs(&env, |j| j.is_empty()).await;
    env.finish().await;
}

/// Tika stand-in: answers like `PUT /tika/text`, and checks that the file name stays on the NAS.
async fn mock_tika() -> url::Url {
    let app = Router::new().route(
        "/tika/text",
        put(|headers: HeaderMap, body: axum::body::Bytes| async move {
            let disposition = headers
                .get("content-disposition")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string();
            assert_eq!(disposition, "attachment; filename=\"dokument.docx\"");
            if body.starts_with(b"PK") {
                (
                    StatusCode::OK,
                    "Protokoll der Eigentümerversammlung".to_string(),
                )
            } else {
                (StatusCode::UNPROCESSABLE_ENTITY, String::new())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}").parse().unwrap()
}

#[tokio::test]
async fn unlesbares_bleibt_ohne_text_und_wird_nicht_wiederholt() {
    if !tools_present() {
        return;
    }
    let Some(mut env) = Env::with_data().await else {
        return;
    };
    let tika = mock_tika().await;
    configure(&mut env, |cfg| cfg.tika_url = Some(tika));
    start(&env).await;
    let (_, root_id, _, dir) = signed_in(&env, "carla").await;
    write(&dir.join("kaputt.pdf"), b"%PDF-1.4 kaputt");
    write(&dir.join("kaputt.docx"), b"kein Zip");
    write(&dir.join("leer.txt"), b"\x00\x01\x02 binaer");
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();
    for _ in 0..600 {
        if sources(&env).await.len() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    wait_for_jobs(&env, |j| j.is_empty()).await;
    let texts: Vec<(String, String)> =
        sqlx::query_as("SELECT source, text FROM content_text ORDER BY source")
            .fetch_all(&env.db.pool)
            .await
            .unwrap();
    assert_eq!(
        texts,
        [
            ("pdf".into(), String::new()),
            ("plain".into(), String::new()),
            ("tika".into(), String::new())
        ]
    );
    env.finish().await;
}

#[tokio::test]
async fn text_nie_unter_falschem_inhalt() {
    if !tools_present() {
        return;
    }
    let Some(env) = Env::with_data().await else {
        return;
    };
    // Index first (it queues the job), workers later.
    search::start(&env.state).await.unwrap();
    let (mut c, root_id, _, dir) = signed_in(&env, "dora").await;
    write(&dir.join("plan.txt"), b"Alpenpanorama");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let first = node(&env, &home, "plan.txt").await.content_hash.unwrap();
    wait_for_jobs(&env, |j| j.len() == 1).await;
    // Changed on disk before the worker reads it, and not scanned yet.
    std::fs::write(dir.join("plan.txt"), b"Bergsee").unwrap();
    extract::start(&env.state).await.unwrap();
    wait_for_jobs(&env, |j| j.len() == 1 && j[0].2.is_some()).await;
    let stored: Vec<Vec<u8>> = sqlx::query_scalar("SELECT hash FROM content_text")
        .fetch_all(&env.db.pool)
        .await
        .unwrap();
    assert!(
        stored.is_empty(),
        "nichts gespeichert, schon gar nicht unter {first:?}"
    );
    // The scan sees the new content; its text arrives under its own hash.
    roots::scan(&env.state, &home).await.unwrap();
    assert_eq!(eventually(&mut c, "Bergsee").await, ["plan.txt"]);
    assert!(names(&mut c, "Alpenpanorama").await.is_empty());
    let second = node(&env, &home, "plan.txt").await.content_hash.unwrap();
    let text: String = sqlx::query_scalar("SELECT text FROM content_text WHERE hash = $1")
        .bind(&second)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(text, "Bergsee");
    env.finish().await;
}

#[tokio::test]
async fn verwaltung_sieht_den_stand() {
    if !tools_present() {
        return;
    }
    let Some(env) = Env::with_data().await else {
        return;
    };
    start(&env).await;
    let invite = env.invite("admin", true).await;
    let (mut admin, _, _) = setup_with_totp(&env, &invite).await;
    let (mut c, root_id, _, dir) = signed_in(&env, "emil").await;
    write(&dir.join("a.txt"), b"Apfel");
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();
    eventually(&mut c, "Apfel").await;
    let s: Value = admin.get("/api/admin/search").await.ok().clone();
    assert_eq!(s["running"], true);
    assert_eq!(s["indexed"], s["journal"]);
    assert_eq!(s["texts"], 1);
    assert_eq!(s["jobs"], json!({"queued": 0, "waiting": 0, "failed": 0}));
    assert_eq!(
        s["extract"],
        json!({"workers": 1, "pdf": true, "ocr": true, "tika": false})
    );
    // Not for everyone.
    assert_eq!(c.get("/api/admin/search").await.status, 403);
    assert_eq!(c.post("/api/admin/jobs/retry", json!({})).await.status, 403);
    // Right after signing in, the second factor is fresh enough.
    let r = admin.post("/api/admin/jobs/retry", json!({})).await;
    assert_eq!(r.ok()["retried"], 0);
    env.finish().await;
}

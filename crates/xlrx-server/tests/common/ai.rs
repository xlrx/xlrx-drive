//! A provider speaking the OpenAI API, on a local port, recording every request.
//!
//! Embeddings are bags of words: every (roughly stemmed) word adds to one dimension. Texts sharing
//! words point in similar directions, so "Rechnung Heizung" finds a heating invoice.

use std::sync::Arc;

use super::files::{node, write};
use super::{Client, Env};
use xlrx_server::AppState;
use xlrx_server::ai::{Side, Space, pipeline, vectors};
use xlrx_server::files::data_class::Class;
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;
use xlrx_server::search::text;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};

/// Dimension when the request does not ask for one (the home network's model).
pub const LOCAL_DIM: usize = 96;
pub const CLOUD_DIM: u32 = 256;
pub const KEY: &str = "test-schluessel";
pub const VISION_MODEL: &str = "gemma-test-vision";
pub const CLIP_MODEL: &str = "clip-test";

#[derive(Clone, Debug)]
pub struct Seen {
    pub path: String,
    pub auth: Option<String>,
    pub body: Value,
}

impl Seen {
    /// The texts sent for embedding.
    pub fn inputs(&self) -> Vec<String> {
        match &self.body["input"] {
            Value::String(s) => vec![s.clone()],
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[derive(Default)]
pub struct Control {
    pub seen: Vec<Seen>,
    /// Answer every request with this status instead.
    pub fail: Option<u16>,
    /// Runs once before the next answer (e.g. a folder becomes "Nur lokal" meanwhile).
    pub before_answer: Option<(sqlx::PgPool, String)>,
}

#[derive(Clone)]
struct Shared(Arc<tokio::sync::Mutex<Control>>);

pub struct FakeAi {
    pub url: url::Url,
    ctl: Arc<tokio::sync::Mutex<Control>>,
    task: tokio::task::JoinHandle<()>,
}

impl FakeAi {
    pub async fn start() -> Self {
        let ctl = Arc::new(tokio::sync::Mutex::new(Control::default()));
        let app = axum::Router::new()
            .route("/v1/embeddings", post(embeddings))
            .route("/v1/chat/completions", post(chat))
            .with_state(Shared(ctl.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self {
            url: format!("http://{addr}/v1").parse().unwrap(),
            ctl,
            task,
        }
    }

    pub async fn seen(&self) -> Vec<Seen> {
        self.ctl.lock().await.seen.clone()
    }

    /// Everything sent, as one text.
    pub async fn sent_text(&self) -> String {
        self.seen()
            .await
            .iter()
            .map(|s| s.body.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub async fn fail_with(&self, status: Option<u16>) {
        self.ctl.lock().await.fail = status;
    }

    pub async fn before_answer(&self, db: sqlx::PgPool, sql: &str) {
        self.ctl.lock().await.before_answer = Some((db, sql.to_owned()));
    }

    pub async fn clear(&self) {
        self.ctl.lock().await.seen.clear();
    }
}

impl Drop for FakeAi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Words meaning the same for the test model (what a real model learns).
const SYNONYMS: &[(&str, &str)] = &[
    ("therme", "heizung"),
    ("heizkessel", "heizung"),
    ("ferien", "urlaub"),
    ("doktor", "arzt"),
    ("welpe", "hund"),
    ("vierbeiner", "hund"),
    ("küste", "strand"),
];

fn stem(w: &str) -> String {
    let w = w.to_lowercase();
    if let Some((_, to)) = SYNONYMS.iter().find(|(from, _)| *from == w) {
        return (*to).to_owned();
    }
    for suffix in ["en", "er", "es", "e", "n", "s"] {
        if w.chars().count() > 4
            && let Some(s) = w.strip_suffix(suffix)
        {
            return s.to_owned();
        }
    }
    w
}

/// The test model's vector of a text (prefixes of the real models removed).
pub fn embed(text: &str, dim: usize) -> Vec<f32> {
    let text = text
        .rsplit("Query: ")
        .next()
        .unwrap_or(text)
        .trim_start_matches("query: ")
        .trim_start_matches("passage: ");
    let mut v = vec![0f32; dim];
    for w in text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
    {
        let h = w_hash(&stem(w));
        v[(h % dim as u64) as usize] += 1.0;
    }
    v[dim - 1] += 0.01;
    v
}

fn w_hash(s: &str) -> u64 {
    // FNV-1a: stable across runs.
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

async fn embeddings(
    State(sh): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut ctl = sh.0.lock().await;
    let seen = Seen {
        path: "/v1/embeddings".into(),
        auth: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: body.clone(),
    };
    let mut inputs = seen.inputs();
    ctl.seen.push(seen);
    if let Some((db, sql)) = ctl.before_answer.take() {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&db)
            .await
            .unwrap();
    }
    if let Some(code) = ctl.fail {
        return (
            StatusCode::from_u16(code).unwrap(),
            Json(json!({"error": {"message": "Testfehler"}})),
        )
            .into_response();
    }
    let dim = body["dimensions"]
        .as_u64()
        .map_or(LOCAL_DIM, |d| d as usize);
    // CLIP: pictures become the words of what they show.
    if body["modality"] == "image" {
        inputs = inputs
            .iter()
            .map(|uri| shows(&picture(uri)).clip.to_owned())
            .collect();
    }
    let tokens: usize = inputs.iter().map(|t| t.split_whitespace().count()).sum();
    let data: Vec<Value> = inputs
        .iter()
        .enumerate()
        .map(|(i, t)| json!({"object": "embedding", "index": i, "embedding": embed(t, dim)}))
        .collect();
    Json(json!({
        "object": "list",
        "model": body["model"],
        "data": data,
        "usage": {"prompt_tokens": tokens, "total_tokens": tokens}
    }))
    .into_response()
}

/// What a test picture shows, by its main colour: red is a dog on the beach, blue a heating
/// invoice, anything else a blurry picture.
pub struct Shows {
    pub vision: Value,
    pub clip: &'static str,
}

pub fn shows(img: &image::RgbImage) -> Shows {
    let n = u64::from((img.width() * img.height()).max(1));
    let sum = |i: usize| img.pixels().map(|p| u64::from(p.0[i])).sum::<u64>() / n;
    let (r, g, b) = (sum(0), sum(1), sum(2));
    if r > g + 60 && r > b + 60 {
        Shows {
            vision: json!({
                "beschreibung": "Ein Hund läuft am Strand entlang, im Hintergrund das Meer.",
                "tags": ["hund", "strand", "meer", "sommer"],
                "text_im_bild": "",
                "dokumenttyp": null,
                "datum_erkannt": null
            }),
            clip: "hund strand meer",
        }
    } else if b > r + 60 && b > g + 60 {
        Shows {
            vision: json!({
                "beschreibung": "Handwerkerrechnung der Firma Müller über die Wartung der Heizung.",
                "tags": ["rechnung", "heizung", "handwerker"],
                "text_im_bild": "Rechnung Nr. 4711 Heizungswartung Betrag 238,00 EUR",
                "dokumenttyp": "rechnung",
                "datum_erkannt": "2025-11-14"
            }),
            clip: "rechnung heizung papier",
        }
    } else {
        Shows {
            vision: json!({"beschreibung": "Ein unscharfes Bild.", "tags": []}),
            clip: "bild",
        }
    }
}

/// The picture in a data URI.
pub fn picture(uri: &str) -> image::RgbImage {
    picture_bytes(uri).1
}

/// Bytes and picture in a data URI.
pub fn picture_bytes(uri: &str) -> (Vec<u8>, image::RgbImage) {
    use base64::Engine as _;
    let b64 = uri.split_once("base64,").expect("data URI").1;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64");
    let img = image::load_from_memory(&bytes).expect("Bild").to_rgb8();
    (bytes, img)
}

async fn chat(State(sh): State<Shared>, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let mut ctl = sh.0.lock().await;
    ctl.seen.push(Seen {
        path: "/v1/chat/completions".into(),
        auth: headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned),
        body: body.clone(),
    });
    if let Some((db, sql)) = ctl.before_answer.take() {
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&db)
            .await
            .unwrap();
    }
    if let Some(code) = ctl.fail {
        return (
            StatusCode::from_u16(code).unwrap(),
            Json(json!({"error": {"message": "Testfehler"}})),
        )
            .into_response();
    }
    let uri = body["messages"][1]["content"][1]["image_url"]["url"]
        .as_str()
        .unwrap_or_default();
    let answer = shows(&picture(uri)).vision;
    Json(json!({
        "choices": [{"index": 0, "message": {"role": "assistant",
            "content": format!("```json\n{answer}\n```")}}],
        "usage": {"prompt_tokens": 280, "completion_tokens": 60, "total_tokens": 340}
    }))
    .into_response()
}

/// A test photo: `w`×`h` in a colour with some noise (so it is not tiny as a file), optionally
/// with EXIF data carrying a "secret" location.
pub fn photo(w: u32, h: u32, rgb: [u8; 3], exif: bool) -> Vec<u8> {
    let mut seed: u32 = 12345 + w + h;
    let img = image::RgbImage::from_fn(w, h, |_, _| {
        let mut px = [0u8; 3];
        for (i, c) in px.iter_mut().enumerate() {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            let noise = ((seed >> 16) % 40) as i32 - 20;
            *c = (i32::from(rgb[i]) + noise).clamp(0, 255) as u8;
        }
        image::Rgb(px)
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode_image(&image::DynamicImage::ImageRgb8(img))
        .unwrap();
    if exif {
        let mut payload = b"Exif\0\0II*\0\x08\0\0\0\0\0\0\0\0\0".to_vec();
        payload.extend_from_slice(b"GPSGEHEIM 48.1372,11.5756");
        let len = (payload.len() + 2) as u16;
        let mut seg = vec![0xFF, 0xE1];
        seg.extend_from_slice(&len.to_be_bytes());
        seg.extend_from_slice(&payload);
        out.splice(2..2, seg);
    }
    out
}

/// Settings for both providers on the fake.
pub fn configure(cfg: &mut xlrx_server::config::Config, fake: &FakeAi) {
    cfg.ai.cloud = Some(xlrx_server::config::CloudAi {
        url: fake.url.clone(),
        key: KEY.into(),
        embed_model: "qwen3-embedding-8b".into(),
        embed_dim: CLOUD_DIM,
        vision_model: Some(VISION_MODEL.into()),
        prices: xlrx_server::config::Prices {
            embed: 0.10,
            vision_in: 0.25,
            vision_out: 0.50,
        },
        max_distance: 0.9,
        spread: 1.0,
    });
    cfg.ai.local = Some(xlrx_server::config::LocalAi {
        url: fake.url.clone(),
        embed_model: "multilingual-e5-small".into(),
        embed_dim: LOCAL_DIM as u32,
        clip_model: Some(CLIP_MODEL.into()),
        clip_dim: LOCAL_DIM as u32,
        max_distance: 0.9,
        clip_max_distance: 0.9,
        spread: 1.0,
        clip_spread: 1.0,
    });
}

// Helpers for the tests of the AI search.

pub async fn env_with(fake: &FakeAi, default: Class) -> Option<Env> {
    let mut env = Env::with_data().await?;
    let mut cfg = env.state.cfg.clone();
    configure(&mut cfg, fake);
    cfg.default_data_class = default;
    env.state = AppState::new(env.db.pool.clone(), cfg).unwrap();
    env.app = xlrx_server::router(env.state.clone());
    Some(env)
}

/// The same environment with other settings (as after a restart).
pub fn restart(env: &mut Env, change: impl FnOnce(&mut xlrx_server::Config)) {
    let mut cfg = env.state.cfg.clone();
    change(&mut cfg);
    env.state = AppState::new(env.db.pool.clone(), cfg).unwrap();
    env.app = xlrx_server::router(env.state.clone());
}

pub async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

/// Writes text files, reads them in and stores their text (as the extraction would).
pub async fn files(env: &Env, root: &RootRow, dir: &std::path::Path, list: &[(&str, &str)]) {
    for (path, content) in list {
        write(&dir.join(path), content.as_bytes());
    }
    roots::scan(&env.state, root).await.unwrap();
    for (path, content) in list {
        let h = hash(env, root, path).await;
        text::store(
            &env.db.pool,
            &h.clone().try_into().unwrap(),
            "plain",
            content,
        )
        .await
        .unwrap();
    }
}

pub async fn hash(env: &Env, root: &RootRow, path: &str) -> Vec<u8> {
    node(env, root, path).await.content_hash.unwrap()
}

pub async fn set_class(c: &mut Client, id: i64, class: &str) {
    let r = c
        .send(
            "PUT",
            &format!("/api/nodes/{id}/data-class"),
            Some(json!({ "class": class })),
        )
        .await;
    r.ok();
}

/// Plans until nothing is new.
pub async fn plan(env: &Env) {
    for _ in 0..20 {
        if !pipeline::plan(&env.state).await.unwrap() {
            return;
        }
    }
    panic!("Planer kommt nicht zum Ende");
}

/// Does the queued work of both sides (a few rounds at most).
pub async fn work(env: &Env) {
    for _ in 0..20 {
        let a = pipeline::work_one(&env.state, Side::Cloud).await.unwrap();
        let b = pipeline::work_one(&env.state, Side::Local).await.unwrap();
        if !a && !b {
            return;
        }
    }
}

pub async fn start_cloud(env: &Env) {
    sqlx::query("UPDATE ai_state SET cloud_on = true")
        .execute(&env.db.pool)
        .await
        .unwrap();
}

/// (kind, key, state, attempts) of the AI jobs.
pub async fn jobs(env: &Env) -> Vec<(String, String, String, i32)> {
    sqlx::query_as(
        "SELECT kind, key, state, attempts FROM jobs WHERE kind LIKE 'ai_%' ORDER BY kind, key",
    )
    .fetch_all(&env.db.pool)
    .await
    .unwrap()
}

pub fn key(h: &[u8]) -> String {
    format!(
        "text:{}",
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

pub fn image_key(h: &[u8]) -> String {
    format!(
        "image:{}",
        h.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}

/// (space, model, pieces) of a content's vectors.
pub async fn vectors_of(env: &Env, h: &[u8]) -> Vec<(String, String, i64)> {
    sqlx::query_as(
        "SELECT space, model, count(*) FROM ai_vectors WHERE content_hash = $1
          GROUP BY space, model ORDER BY space",
    )
    .bind(h)
    .fetch_all(&env.db.pool)
    .await
    .unwrap()
}

pub fn cloud_vec(n: i64) -> Vec<(String, String, i64)> {
    vec![("cloud".into(), "qwen3-embedding-8b".into(), n)]
}

pub fn local_vec(n: i64) -> Vec<(String, String, i64)> {
    vec![("local".into(), "multilingual-e5-small".into(), n)]
}

pub async fn nearest(env: &Env, space: Space, q: &str) -> Vec<Vec<u8>> {
    let (model, dim) = env.state.ai.model(space).unwrap();
    let mut v = embed(q, dim as usize);
    vectors::normalize(&mut v);
    vectors::nearest(&env.db.pool, space, model, dim, &v, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|n| n.content_hash)
        .collect()
}

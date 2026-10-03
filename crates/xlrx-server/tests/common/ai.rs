//! A provider speaking the OpenAI API, on a local port, recording every request.
//!
//! Embeddings are bags of words: every (roughly stemmed) word adds to one dimension. Texts sharing
//! words point in similar directions, so "Rechnung Heizung" finds a heating invoice.

use std::sync::Arc;

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

fn stem(w: &str) -> String {
    let w = w.to_lowercase();
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
    let inputs = seen.inputs();
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

/// Settings for both providers on the fake.
pub fn configure(cfg: &mut xlrx_server::config::Config, fake: &FakeAi) {
    cfg.ai.cloud = Some(xlrx_server::config::CloudAi {
        url: fake.url.clone(),
        key: KEY.into(),
        embed_model: "qwen3-embedding-8b".into(),
        embed_dim: CLOUD_DIM,
        vision_model: None,
        prices: xlrx_server::config::Prices {
            embed: 0.10,
            vision_in: 0.25,
            vision_out: 0.50,
        },
    });
    cfg.ai.local = Some(xlrx_server::config::LocalAi {
        url: fake.url.clone(),
        embed_model: "multilingual-e5-small".into(),
        embed_dim: LOCAL_DIM as u32,
        clip_model: None,
        clip_dim: 512,
    });
}

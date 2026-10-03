//! `POST /v1/embeddings` as OpenAI (and Infinity, for pictures) speak it:
//! `{"model": …, "input": "…" | ["…"], "modality": "text" | "image"}`; pictures as data URIs.
//! `GET /health`, `GET /v1/models`.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::models::{ClipText, ClipVision, TextModel};

/// Most inputs per request, characters per text, bytes per picture.
const MAX_INPUTS: usize = 64;
const MAX_CHARS: usize = 100_000;
const MAX_PICTURE: usize = 32 * 1024 * 1024;

pub struct Models {
    /// Name and model for texts (e.g. `multilingual-e5-small`).
    pub text: Option<(String, Box<dyn TextModel>)>,
    /// Name and models for pictures and the texts searching them.
    pub clip: Option<(String, ClipText, ClipVision)>,
    /// One computation at a time: the NAS has few cores and the models use all of them.
    pub busy: tokio::sync::Semaphore,
}

pub fn router(models: Arc<Models>) -> axum::Router {
    axum::Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/models", get(list))
        .route("/v1/embeddings", post(embeddings))
        .layer(axum::extract::DefaultBodyLimit::max(64 * 1024 * 1024))
        .with_state(models)
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message}}))).into_response()
}

async fn list(State(m): State<Arc<Models>>) -> Json<Value> {
    let names: Vec<&str> = m
        .text
        .iter()
        .map(|(n, _)| n.as_str())
        .chain(m.clip.iter().map(|(n, _, _)| n.as_str()))
        .collect();
    Json(json!({
        "object": "list",
        "data": names.iter().map(|n| json!({"id": n, "object": "model"})).collect::<Vec<_>>()
    }))
}

#[derive(Deserialize)]
pub struct Request {
    model: String,
    input: Input,
    #[serde(default)]
    modality: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Input {
    One(String),
    Many(Vec<String>),
}

/// The bytes of a picture sent as data URI (`data:image/jpeg;base64,…`).
pub fn data_uri(uri: &str) -> Result<Vec<u8>, String> {
    let rest = uri
        .strip_prefix("data:")
        .ok_or("Bild als data:-URI erwartet")?;
    let (meta, b64) = rest.split_once(',').ok_or("data:-URI ohne Inhalt")?;
    if !meta.starts_with("image/") || !meta.ends_with(";base64") {
        return Err("data:-URI muss ein Base64-Bild sein".into());
    }
    if b64.len() > MAX_PICTURE * 4 / 3 + 4 {
        return Err("Bild zu groß".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| "Base64 nicht lesbar".to_string())
}

/// Decodes a picture with limits (no "decompression bombs").
pub fn decode(bytes: &[u8]) -> Result<image::DynamicImage, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(12_000);
    limits.max_image_height = Some(12_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .map_err(|e| format!("Bild nicht lesbar: {e}"))
}

/// The vectors and the tokens read, or what went wrong.
type Computed = Result<(Vec<Vec<f32>>, usize), (StatusCode, String)>;

async fn embeddings(State(m): State<Arc<Models>>, Json(req): Json<Request>) -> Response {
    let inputs = match req.input {
        Input::One(s) => vec![s],
        Input::Many(v) => v,
    };
    if inputs.is_empty() || inputs.len() > MAX_INPUTS {
        return error(StatusCode::BAD_REQUEST, "1 bis 64 Eingaben");
    }
    let picture = match req.modality.as_deref() {
        None | Some("text") => false,
        Some("image") => true,
        Some(_) => return error(StatusCode::BAD_REQUEST, "modality: text oder image"),
    };
    let is_text = m.text.as_ref().is_some_and(|(n, _)| *n == req.model);
    let is_clip = m.clip.as_ref().is_some_and(|(n, _, _)| *n == req.model);
    if !is_text && !is_clip {
        return error(
            StatusCode::NOT_FOUND,
            &format!("Unbekanntes Modell „{}“", req.model),
        );
    }
    if picture && !is_clip {
        return error(StatusCode::BAD_REQUEST, "Bilder nur mit dem CLIP-Modell");
    }
    if !picture && inputs.iter().any(|t| t.chars().count() > MAX_CHARS) {
        return error(StatusCode::PAYLOAD_TOO_LARGE, "Text zu lang");
    }
    let Ok(_permit) = m.busy.acquire().await else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "wird beendet");
    };
    let model_name = req.model.clone();
    let mm = m.clone();
    let done = tokio::task::spawn_blocking(move || -> Computed {
        let mut vectors = Vec::with_capacity(inputs.len());
        let mut tokens = 0;
        for input in &inputs {
            let v = if picture {
                let (_, _, vision) = mm.clip.as_ref().expect("checked");
                let bytes = data_uri(input).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
                let img = decode(&bytes).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
                vision
                    .embed(&img)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?
            } else {
                let model: &dyn TextModel = if is_text {
                    mm.text.as_ref().expect("checked").1.as_ref()
                } else {
                    &mm.clip.as_ref().expect("checked").1
                };
                let (v, n) = model
                    .embed(input)
                    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
                tokens += n;
                v
            };
            vectors.push(v);
        }
        Ok((vectors, tokens))
    })
    .await;
    match done {
        Ok(Ok((vectors, tokens))) => Json(json!({
            "object": "list",
            "model": model_name,
            "data": vectors.into_iter().enumerate().map(|(i, v)| json!({
                "object": "embedding", "index": i, "embedding": v
            })).collect::<Vec<_>>(),
            "usage": {"prompt_tokens": tokens, "total_tokens": tokens}
        }))
        .into_response(),
        Ok(Err((status, message))) => error(status, &message),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pictures_as_data_uris() {
        let png = {
            let mut out = Vec::new();
            image::RgbImage::from_pixel(4, 4, image::Rgb([1, 2, 3]))
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        let uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png)
        );
        assert_eq!(data_uri(&uri).unwrap(), png);
        assert_eq!(decode(&png).unwrap().width(), 4);
        assert!(data_uri("https://example.com/a.jpg").is_err());
        assert!(data_uri("data:text/plain;base64,aGFsbG8=").is_err());
        assert!(data_uri("data:image/png;base64,***").is_err());
        assert!(decode(b"kein Bild").is_err());
    }
}

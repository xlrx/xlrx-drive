//! Providers speaking the OpenAI API (PLAN 7.1): the cloud for "Cloud erlaubt" (Scaleway or
//! another EU provider) and `embed-local` in the home network for "Nur lokal".
//!
//! - The home-network service is reached only at addresses in the home network: checked when
//!   configuring, and on every connection for every address a name resolves to.
//! - The cloud is reached over HTTPS (plain HTTP only to a proxy in the home network, checked the
//!   same way).
//! - Contents go to the cloud only with a [`Cleared`]: proof that they were checked just before.
//! - Redirects are not followed: a request never goes anywhere else.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use url::{Host, Url};

use crate::config::{CloudAi, LocalAi, Prices};
use crate::extract::tika::is_local;

/// Largest answer read (embeddings of many pieces, picture descriptions).
const MAX_ANSWER: usize = 32 * 1024 * 1024;
/// Pieces sent in one request.
const BATCH: usize = 32;
/// Embedding a document's pieces: the cloud answers quickly, the NAS needs a while per piece.
const CLOUD_TIME: Duration = Duration::from_secs(120);
const HOME_TIME: Duration = Duration::from_secs(15 * 60);

/// Does the host lie in the home network as far as can be told without asking DNS: an address
/// there, `localhost` or a name without dots (a container on the same Docker network)?
fn home_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(ip)) => is_local(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_local(IpAddr::V6(ip)),
        Some(Host::Domain(name)) => name == "localhost" || !name.contains('.'),
        None => false,
    }
}

fn plain(var: &str, url: &Url) -> Result<(), String> {
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(format!("{var}: ohne Zugangsdaten und Parameter angeben"));
    }
    Ok(())
}

/// The cloud provider's address: HTTPS, or HTTP to a proxy in the home network.
pub fn check_cloud_url(url: &Url) -> Result<(), String> {
    plain("XLRX_AI_URL", url)?;
    match url.scheme() {
        "https" => Ok(()),
        "http" if home_host(url) => Ok(()),
        _ => Err("XLRX_AI_URL: nur https:// (z. B. https://api.scaleway.ai/v1)".into()),
    }
}

/// The address of a service that must be in the home network (`embed-local`).
pub fn check_home_url(var: &str, url: &Url) -> Result<(), String> {
    plain(var, url)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!("{var}: http:// oder https://"));
    }
    let literal_outside = match url.host() {
        Some(Host::Ipv4(ip)) => !is_local(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => !is_local(IpAddr::V6(ip)),
        Some(Host::Domain(_)) => false,
        None => true,
    };
    if literal_outside {
        return Err(format!(
            "{var}: {url} liegt nicht im Heimnetz. Inhalte aus „Nur lokal“ dürfen das Heimnetz nicht verlassen."
        ));
    }
    Ok(())
}

/// Resolves names; for the home network, refuses unless every address lies there (literal
/// addresses are checked when configuring: the connector does not resolve them).
#[derive(Clone)]
struct Resolver {
    home_only: bool,
}

type Resolving =
    Pin<Box<dyn Future<Output = std::io::Result<std::vec::IntoIter<SocketAddr>>> + Send>>;

impl tower::Service<Name> for Resolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = std::io::Error;
    type Future = Resolving;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let home_only = self.home_only;
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let addrs: Vec<SocketAddr> =
                tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
            if home_only && (addrs.is_empty() || addrs.iter().any(|a| !is_local(a.ip()))) {
                return Err(std::io::Error::other(format!(
                    "{host} löst nicht nur zu Adressen im Heimnetz auf – nichts gesendet"
                )));
            }
            Ok(addrs.into_iter())
        })
    }
}

type Https = hyper_rustls::HttpsConnector<HttpConnector<Resolver>>;

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// Worth trying again later: not reachable, overloaded (429, 5xx), time limit.
    Transient(String),
    /// The provider refuses the access (401, 403): the key or the plan needs looking at.
    Access(String),
    /// The provider refuses this input (400, 413, 422 …): trying again will not help.
    Rejected(String),
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallError::Transient(m) | CallError::Access(m) | CallError::Rejected(m) => {
                f.write_str(m)
            }
        }
    }
}

/// One provider address with its key.
pub struct Endpoint {
    base: Url,
    key: Option<String>,
    client: Client<Https, Full<Bytes>>,
}

impl Endpoint {
    fn new(url: &Url, key: Option<String>, home_only: bool) -> Result<Self, String> {
        let mut base = url.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let mut http = HttpConnector::new_with_resolver(Resolver { home_only });
        http.enforce_http(false);
        http.set_connect_timeout(Some(Duration::from_secs(10)));
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .map_err(|e| format!("TLS: {e}"))?
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        Ok(Self {
            base,
            key,
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    /// Posts JSON to `path` (relative to the base, e.g. `embeddings`) and returns the answer.
    pub async fn post(
        &self,
        path: &str,
        body: &Value,
        limit: Duration,
    ) -> Result<Value, CallError> {
        tokio::time::timeout(limit, self.request(path, body))
            .await
            .map_err(|_| CallError::Transient(format!("Zeitlimit von {} s", limit.as_secs())))?
    }

    async fn request(&self, path: &str, body: &Value) -> Result<Value, CallError> {
        let transient = |e: &dyn std::fmt::Display| CallError::Transient(e.to_string());
        let url = self.base.join(path).map_err(|e| transient(&e))?;
        let mut req = hyper::Request::post(url.as_str())
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .header(hyper::header::ACCEPT, "application/json");
        if let Some(k) = &self.key {
            req = req.header(hyper::header::AUTHORIZATION, format!("Bearer {k}"));
        }
        let req = req
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| transient(&e))?;
        let res = self.client.request(req).await.map_err(|e| {
            // The connection error says why (e.g. an address outside the home network).
            let mut msg = e.to_string();
            let mut source = std::error::Error::source(&e);
            while let Some(s) = source {
                msg = format!("{msg}: {s}");
                source = s.source();
            }
            CallError::Transient(msg)
        })?;
        let status = res.status();
        let bytes = Limited::new(res.into_body(), MAX_ANSWER)
            .collect()
            .await
            .map_err(|e| transient(&e))?
            .to_bytes();
        if !status.is_success() {
            let detail: String = String::from_utf8_lossy(&bytes).chars().take(300).collect();
            let msg = format!("Antwort {status}: {}", detail.trim());
            return Err(match status.as_u16() {
                401 | 403 => CallError::Access(msg),
                408 | 409 | 425 | 429 => CallError::Transient(msg),
                400..=499 => CallError::Rejected(msg),
                _ => CallError::Transient(msg),
            });
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| CallError::Transient(format!("Antwort ist kein JSON: {e}")))
    }
}

/// What a model expects in front of queries and documents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prefixes {
    pub query: String,
    pub document: String,
}

impl Prefixes {
    /// The prefixes the known model families were trained with.
    pub fn of(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        let (query, document) = if m.contains("e5") {
            ("query: ", "passage: ")
        } else if m.contains("qwen3-embedding") {
            (
                "Instruct: Given a web search query, retrieve relevant passages that answer the query\nQuery: ",
                "",
            )
        } else if m.contains("embeddinggemma") {
            ("task: search result | query: ", "title: none | text: ")
        } else {
            ("", "")
        };
        Self {
            query: query.into(),
            document: document.into(),
        }
    }
}

/// Vectors and what they cost.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Embedded {
    pub vectors: Vec<Vec<f32>>,
    pub tokens: u64,
}

/// An embedding model at a provider (texts; for CLIP also pictures).
pub struct Embedder {
    ep: Arc<Endpoint>,
    pub model: String,
    pub dim: u32,
    /// Ask for the dimension (models with selectable size, e.g. qwen3-embedding).
    ask_dim: bool,
    prefixes: Prefixes,
    time: Duration,
}

impl Embedder {
    /// Embeds inputs in batches. `modality` tells a CLIP service whether they are texts or
    /// pictures (as data URIs).
    async fn embed(
        &self,
        inputs: Vec<String>,
        modality: Option<&str>,
        limit: Duration,
    ) -> Result<Embedded, CallError> {
        let mut out = Embedded::default();
        for batch in inputs.chunks(BATCH) {
            let mut body = json!({
                "model": self.model,
                "input": batch,
                "encoding_format": "float",
            });
            if self.ask_dim {
                body["dimensions"] = json!(self.dim);
            }
            if let Some(m) = modality {
                body["modality"] = json!(m);
            }
            let answer = self.ep.post("embeddings", &body, limit).await?;
            let (vectors, tokens) = parse_embeddings(&answer, batch.len(), self.dim as usize)?;
            out.vectors.extend(vectors);
            out.tokens += tokens.unwrap_or_else(|| estimate_tokens(batch));
        }
        Ok(out)
    }

    async fn documents(&self, texts: &[String]) -> Result<Embedded, CallError> {
        let p = &self.prefixes.document;
        let inputs = texts.iter().map(|t| format!("{p}{t}")).collect();
        self.embed(inputs, None, self.time).await
    }

    /// Embeds a search query.
    pub async fn query(&self, q: &str, limit: Duration) -> Result<Embedded, CallError> {
        let input = format!("{}{q}", self.prefixes.query);
        self.embed(vec![input], None, limit).await
    }
}

/// Rough number of tokens of texts (when the provider does not say).
pub fn estimate_tokens(texts: &[String]) -> u64 {
    texts.iter().map(|t| t.chars().count() as u64 / 3 + 1).sum()
}

/// Reads an `/embeddings` answer: as many vectors as inputs, in order, cut to `dim` and
/// normalized (models with nested dimensions, "Matryoshka", may answer longer ones).
fn parse_embeddings(
    answer: &Value,
    count: usize,
    dim: usize,
) -> Result<(Vec<Vec<f32>>, Option<u64>), CallError> {
    let bad = |m: &str| CallError::Transient(format!("Unerwartete Antwort: {m}"));
    let data = answer["data"].as_array().ok_or_else(|| bad("data fehlt"))?;
    if data.len() != count {
        return Err(bad(&format!("{} statt {count} Vektoren", data.len())));
    }
    let mut out: Vec<Option<Vec<f32>>> = vec![None; count];
    for (i, item) in data.iter().enumerate() {
        let index = item["index"].as_u64().map_or(i, |x| x as usize);
        let values = item["embedding"]
            .as_array()
            .ok_or_else(|| bad("embedding fehlt"))?;
        if values.len() < dim {
            return Err(bad(&format!(
                "Vektor mit {} statt {dim} Dimensionen",
                values.len()
            )));
        }
        let mut v: Vec<f32> = values[..dim]
            .iter()
            .map(|x| x.as_f64().map(|f| f as f32))
            .collect::<Option<_>>()
            .ok_or_else(|| bad("keine Zahl im Vektor"))?;
        if !crate::ai::vectors::normalize(&mut v) {
            return Err(bad("leerer Vektor"));
        }
        *out.get_mut(index).ok_or_else(|| bad("Index außerhalb"))? = Some(v);
    }
    let vectors = out
        .into_iter()
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| bad("Vektor fehlt"))?;
    let usage = &answer["usage"];
    let tokens = usage["prompt_tokens"]
        .as_u64()
        .or_else(|| usage["total_tokens"].as_u64());
    Ok((vectors, tokens))
}

/// Proof that a content may go to the cloud, checked just before sending (see
/// [`super::clear`]). Only that function makes one.
pub struct Cleared {
    pub(super) _private: (),
}

/// A picture as sent: scaled down, encoded anew (no EXIF, no GPS).
pub struct Picture {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
}

impl Picture {
    fn data_uri(&self) -> String {
        use base64::Engine as _;
        format!(
            "data:{};base64,{}",
            self.mime,
            base64::engine::general_purpose::STANDARD.encode(&self.bytes)
        )
    }
}

/// What a vision model saw in a picture (PLAN 7.2).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Seen {
    pub description: String,
    pub tags: Vec<String>,
    pub text: String,
    pub doc_type: Option<String>,
    pub date: Option<time::Date>,
    pub tokens_in: u64,
    pub tokens_out: u64,
}

impl Seen {
    /// What is embedded and indexed: description, tags and the text in the picture.
    pub fn words(&self) -> String {
        let mut out = self.description.clone();
        if !self.tags.is_empty() {
            out.push('\n');
            out.push_str(&self.tags.join(", "));
        }
        if !self.text.is_empty() {
            out.push('\n');
            out.push_str(&self.text);
        }
        out
    }
}

/// The instructions for the vision model. Names of people are not wanted; the answer is JSON.
const VISION_PROMPT: &str = "Du beschreibst Bilder für die Suche in einer privaten Dateiablage. \
Antworte nur mit einem JSON-Objekt mit diesen Feldern:\n\
\"beschreibung\": ein bis zwei Sätze auf Deutsch, was zu sehen ist (Motiv, Ort, Situation; bei \
Dokumenten Art, Absender und Thema),\n\
\"tags\": 3 bis 10 deutsche Schlagwörter in Kleinbuchstaben,\n\
\"text_im_bild\": gut lesbarer Text im Bild, höchstens 2000 Zeichen, sonst \"\",\n\
\"dokumenttyp\": bei Dokumenten eines von rechnung, vertrag, brief, kontoauszug, quittung, \
ausweis, zeugnis, bescheid, formular, rezept, fahrkarte, sonstiges; sonst null,\n\
\"datum_erkannt\": ein im Bild erkennbares Datum als JJJJ-MM-TT, sonst null.\n\
Nenne keine Personen beim Namen.";

/// Longest values taken from a vision answer.
const MAX_DESCRIPTION: usize = 1000;
const MAX_IMAGE_TEXT: usize = 4000;
const MAX_TAGS: usize = 15;

fn cut(s: &str, max: usize) -> String {
    s.trim().chars().take(max).collect()
}

/// Reads the vision model's answer: the JSON object in it (also inside a code fence); without
/// one, the text is the description.
pub fn parse_seen(answer: &Value) -> Result<Seen, CallError> {
    let content = &answer["choices"][0]["message"]["content"];
    let text = match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p["text"].as_str())
            .collect::<Vec<_>>()
            .join(""),
        _ => {
            return Err(CallError::Transient(
                "Unerwartete Antwort: keine Nachricht".into(),
            ));
        }
    };
    let usage = &answer["usage"];
    let mut seen = Seen {
        tokens_in: usage["prompt_tokens"].as_u64().unwrap_or(0),
        tokens_out: usage["completion_tokens"].as_u64().unwrap_or(0),
        ..Seen::default()
    };
    let json = match (text.find('{'), text.rfind('}')) {
        (Some(a), Some(b)) if a < b => serde_json::from_str::<Value>(&text[a..=b]).ok(),
        _ => None,
    };
    let Some(j) = json else {
        seen.description = cut(&text, MAX_DESCRIPTION);
        return Ok(seen);
    };
    seen.description = cut(
        j["beschreibung"].as_str().unwrap_or_default(),
        MAX_DESCRIPTION,
    );
    seen.tags = j["tags"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str())
                .map(|t| cut(&t.to_lowercase(), 40))
                .filter(|t| !t.is_empty())
                .take(MAX_TAGS)
                .collect()
        })
        .unwrap_or_default();
    seen.text = cut(
        j["text_im_bild"].as_str().unwrap_or_default(),
        MAX_IMAGE_TEXT,
    );
    seen.doc_type = j["dokumenttyp"]
        .as_str()
        .map(|t| t.trim().to_lowercase())
        .filter(|t| {
            !t.is_empty()
                && t != "null"
                && t.chars().count() <= 30
                && t.chars().all(|c| c.is_alphabetic() || c == '-')
        });
    seen.date = j["datum_erkannt"].as_str().and_then(|d| {
        time::Date::parse(
            d.trim(),
            time::macros::format_description!("[year]-[month]-[day]"),
        )
        .ok()
    });
    Ok(seen)
}

/// The vision model in the cloud.
pub struct Vision {
    ep: Arc<Endpoint>,
    pub model: String,
}

/// The provider for "Cloud erlaubt".
pub struct Cloud {
    pub embed: Embedder,
    pub vision: Option<Vision>,
    pub prices: Prices,
}

impl Cloud {
    pub fn new(cfg: &CloudAi) -> Result<Self, String> {
        check_cloud_url(&cfg.url)?;
        // Plain HTTP only goes to a proxy in the home network.
        let home_only = cfg.url.scheme() == "http";
        let ep = Arc::new(Endpoint::new(&cfg.url, Some(cfg.key.clone()), home_only)?);
        Ok(Self {
            embed: Embedder {
                ep: ep.clone(),
                model: cfg.embed_model.clone(),
                dim: cfg.embed_dim,
                ask_dim: true,
                prefixes: Prefixes::of(&cfg.embed_model),
                time: CLOUD_TIME,
            },
            vision: cfg.vision_model.clone().map(|model| Vision { ep, model }),
            prices: cfg.prices,
        })
    }

    /// Embeds pieces of a content that was cleared for the cloud.
    pub async fn embed_content(
        &self,
        _: &Cleared,
        texts: &[String],
    ) -> Result<Embedded, CallError> {
        self.embed.documents(texts).await
    }

    /// Asks the vision model what a picture of a cleared content shows.
    pub async fn describe(&self, _: &Cleared, picture: &Picture) -> Result<Seen, CallError> {
        let vision = self
            .vision
            .as_ref()
            .ok_or_else(|| CallError::Rejected("Kein Bildmodell eingerichtet".into()))?;
        let body = json!({
            "model": vision.model,
            "messages": [
                {"role": "system", "content": VISION_PROMPT},
                {"role": "user", "content": [
                    {"type": "text", "text": "Beschreibe dieses Bild."},
                    {"type": "image_url", "image_url": {"url": picture.data_uri()}}
                ]}
            ],
            "max_tokens": 1200,
            "temperature": 0.1,
        });
        let answer = vision
            .ep
            .post("chat/completions", &body, CLOUD_TIME)
            .await?;
        parse_seen(&answer)
    }

    /// What tokens cost in euros.
    pub fn embed_cost(&self, tokens: u64) -> f64 {
        tokens as f64 / 1e6 * self.prices.embed
    }

    pub fn vision_cost(&self, tokens_in: u64, tokens_out: u64) -> f64 {
        tokens_in as f64 / 1e6 * self.prices.vision_in
            + tokens_out as f64 / 1e6 * self.prices.vision_out
    }
}

/// The service in the home network for "Nur lokal".
pub struct Local {
    pub embed: Embedder,
    /// Picture vectors (CLIP: pictures and texts in one space).
    pub clip: Option<Embedder>,
}

impl Local {
    pub fn new(cfg: &LocalAi) -> Result<Self, String> {
        check_home_url("XLRX_LOCAL_AI_URL", &cfg.url)?;
        let ep = Arc::new(Endpoint::new(&cfg.url, None, true)?);
        Ok(Self {
            embed: Embedder {
                ep: ep.clone(),
                model: cfg.embed_model.clone(),
                dim: cfg.embed_dim,
                ask_dim: false,
                prefixes: Prefixes::of(&cfg.embed_model),
                time: HOME_TIME,
            },
            clip: cfg.clip_model.clone().map(|model| Embedder {
                ep,
                model,
                dim: cfg.clip_dim,
                ask_dim: false,
                prefixes: Prefixes {
                    query: String::new(),
                    document: String::new(),
                },
                time: HOME_TIME,
            }),
        })
    }

    /// Embeds pieces of a content (everything stays in the home network).
    pub async fn embed_content(&self, texts: &[String]) -> Result<Embedded, CallError> {
        self.embed.documents(texts).await
    }

    /// The CLIP vector of a picture.
    pub async fn embed_picture(&self, picture: &Picture) -> Result<Vec<f32>, CallError> {
        let clip = self
            .clip
            .as_ref()
            .ok_or_else(|| CallError::Rejected("Kein Bildmodell eingerichtet".into()))?;
        let mut e = clip
            .embed(vec![picture.data_uri()], Some("image"), clip.time)
            .await?;
        e.vectors
            .pop()
            .ok_or_else(|| CallError::Transient("Kein Vektor".into()))
    }

    /// The CLIP vector of a search query (the text encoder: comparable with pictures).
    pub async fn clip_query(&self, q: &str, limit: Duration) -> Result<Embedded, CallError> {
        let clip = self
            .clip
            .as_ref()
            .ok_or_else(|| CallError::Rejected("Kein Bildmodell eingerichtet".into()))?;
        clip.embed(vec![q.to_owned()], Some("text"), limit).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_ok(u: &str) -> bool {
        check_cloud_url(&u.parse().unwrap()).is_ok()
    }

    fn home_ok(u: &str) -> bool {
        check_home_url("X", &u.parse().unwrap()).is_ok()
    }

    #[test]
    fn addresses() {
        assert!(cloud_ok("https://api.scaleway.ai/v1"));
        assert!(cloud_ok("http://litellm:4000/v1"));
        assert!(cloud_ok("http://127.0.0.1:4000/v1"));
        assert!(!cloud_ok("http://api.scaleway.ai/v1"));
        assert!(!cloud_ok("http://8.8.8.8/v1"));
        assert!(!cloud_ok("https://user:pw@api.scaleway.ai/v1"));
        assert!(!cloud_ok("https://api.scaleway.ai/v1?key=1"));

        assert!(home_ok("http://embed-local:8090/v1"));
        assert!(home_ok("http://192.168.1.20:8090/v1"));
        assert!(home_ok("http://[fd00::4]:8090/v1"));
        // Names are checked on every connection.
        assert!(home_ok("http://mac-mini.fritz.box:8090/v1"));
        assert!(!home_ok("http://8.8.8.8:8090/v1"));
        assert!(!home_ok("http://[2001:db8::1]:8090/v1"));
        assert!(!home_ok("ftp://embed-local/v1"));
    }

    #[test]
    fn prefixes_per_model() {
        assert_eq!(Prefixes::of("multilingual-e5-small").query, "query: ");
        assert_eq!(Prefixes::of("multilingual-e5-small").document, "passage: ");
        assert!(
            Prefixes::of("qwen3-embedding-8b")
                .query
                .starts_with("Instruct:")
        );
        assert_eq!(Prefixes::of("qwen3-embedding-8b").document, "");
        assert_eq!(Prefixes::of("bge-m3"), Prefixes::of("unbekannt"));
    }

    #[test]
    fn vision_answers() {
        let a = json!({
            "choices": [{"message": {"content": "```json\n{\"beschreibung\": \"Ein Hund am Strand.\", \"tags\": [\"Hund\", \"strand\", 3], \"text_im_bild\": \"\", \"dokumenttyp\": null, \"datum_erkannt\": \"2025-11-14\"}\n```"}}],
            "usage": {"prompt_tokens": 300, "completion_tokens": 40}
        });
        let s = parse_seen(&a).unwrap();
        assert_eq!(s.description, "Ein Hund am Strand.");
        assert_eq!(s.tags, ["hund", "strand"]);
        assert_eq!(s.doc_type, None);
        assert_eq!(s.date, Some(time::macros::date!(2025 - 11 - 14)));
        assert_eq!((s.tokens_in, s.tokens_out), (300, 40));
        assert_eq!(s.words(), "Ein Hund am Strand.\nhund, strand");

        let doc = json!({"choices": [{"message": {"content": "{\"beschreibung\": \"Rechnung\", \"dokumenttyp\": \"Rechnung\", \"datum_erkannt\": \"14.11.2025\"}"}}]});
        let s = parse_seen(&doc).unwrap();
        assert_eq!(s.doc_type.as_deref(), Some("rechnung"));
        assert_eq!(s.date, None);
        let odd = json!({"choices": [{"message": {"content": "{\"beschreibung\": \"x\", \"dokumenttyp\": \"<b>rechnung</b>\"}"}}]});
        assert_eq!(parse_seen(&odd).unwrap().doc_type, None);
        let prose = json!({"choices": [{"message": {"content": "Ein Sonnenuntergang am Meer."}}]});
        assert_eq!(
            parse_seen(&prose).unwrap().description,
            "Ein Sonnenuntergang am Meer."
        );
        assert!(parse_seen(&json!({"choices": []})).is_err());
    }

    #[test]
    fn answers() {
        let a = json!({
            "data": [
                {"index": 1, "embedding": [0.0, 3.0, 4.0, 9.0]},
                {"index": 0, "embedding": [1.0, 0.0, 0.0, 9.0]}
            ],
            "usage": {"prompt_tokens": 7, "total_tokens": 7}
        });
        let (v, t) = parse_embeddings(&a, 2, 3).unwrap();
        assert_eq!(t, Some(7));
        assert_eq!(v[0], vec![1.0, 0.0, 0.0]);
        assert!((v[1][1] - 0.6).abs() < 1e-6 && (v[1][2] - 0.8).abs() < 1e-6);
        assert!(parse_embeddings(&a, 3, 3).is_err());
        assert!(parse_embeddings(&a, 2, 5).is_err());
        let zero = json!({"data": [{"index": 0, "embedding": [0.0, 0.0]}]});
        assert!(parse_embeddings(&zero, 1, 2).is_err());
    }
}

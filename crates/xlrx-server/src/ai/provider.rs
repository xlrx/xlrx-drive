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

/// A text embedding model at a provider.
pub struct Embedder {
    ep: Endpoint,
    pub model: String,
    pub dim: u32,
    /// Ask for the dimension (models with selectable size, e.g. qwen3-embedding).
    ask_dim: bool,
    prefixes: Prefixes,
    time: Duration,
}

impl Embedder {
    /// Embeds texts (prefixed as documents or queries), in batches.
    async fn embed(
        &self,
        texts: &[String],
        query: bool,
        limit: Duration,
    ) -> Result<Embedded, CallError> {
        let prefix = if query {
            &self.prefixes.query
        } else {
            &self.prefixes.document
        };
        let mut out = Embedded::default();
        for batch in texts.chunks(BATCH) {
            let input: Vec<String> = batch.iter().map(|t| format!("{prefix}{t}")).collect();
            let mut body = json!({
                "model": self.model,
                "input": input,
                "encoding_format": "float",
            });
            if self.ask_dim {
                body["dimensions"] = json!(self.dim);
            }
            let answer = self.ep.post("embeddings", &body, limit).await?;
            let (vectors, tokens) = parse_embeddings(&answer, input.len(), self.dim as usize)?;
            out.vectors.extend(vectors);
            out.tokens += tokens.unwrap_or_else(|| estimate_tokens(&input));
        }
        Ok(out)
    }

    /// Embeds a search query.
    pub async fn query(&self, q: &str, limit: Duration) -> Result<Embedded, CallError> {
        self.embed(&[q.to_owned()], true, limit).await
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

/// The provider for "Cloud erlaubt".
pub struct Cloud {
    pub embed: Embedder,
    pub prices: Prices,
}

impl Cloud {
    pub fn new(cfg: &CloudAi) -> Result<Self, String> {
        check_cloud_url(&cfg.url)?;
        // Plain HTTP only goes to a proxy in the home network.
        let home_only = cfg.url.scheme() == "http";
        Ok(Self {
            embed: Embedder {
                ep: Endpoint::new(&cfg.url, Some(cfg.key.clone()), home_only)?,
                model: cfg.embed_model.clone(),
                dim: cfg.embed_dim,
                ask_dim: true,
                prefixes: Prefixes::of(&cfg.embed_model),
                time: CLOUD_TIME,
            },
            prices: cfg.prices,
        })
    }

    /// Embeds pieces of a content that was cleared for the cloud.
    pub async fn embed_content(
        &self,
        _: &Cleared,
        texts: &[String],
    ) -> Result<Embedded, CallError> {
        self.embed.embed(texts, false, self.embed.time).await
    }

    /// What tokens cost in euros.
    pub fn embed_cost(&self, tokens: u64) -> f64 {
        tokens as f64 / 1e6 * self.prices.embed
    }
}

/// The service in the home network for "Nur lokal".
pub struct Local {
    pub embed: Embedder,
}

impl Local {
    pub fn new(cfg: &LocalAi) -> Result<Self, String> {
        check_home_url("XLRX_LOCAL_AI_URL", &cfg.url)?;
        Ok(Self {
            embed: Embedder {
                ep: Endpoint::new(&cfg.url, None, true)?,
                model: cfg.embed_model.clone(),
                dim: cfg.embed_dim,
                ask_dim: false,
                prefixes: Prefixes::of(&cfg.embed_model),
                time: HOME_TIME,
            },
        })
    }

    /// Embeds pieces of a content (everything stays in the home network).
    pub async fn embed_content(&self, texts: &[String]) -> Result<Embedded, CallError> {
        self.embed.embed(texts, false, self.embed.time).await
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

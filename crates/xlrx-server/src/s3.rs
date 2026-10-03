//! A small S3 client for the outside cache (PLAN 15.2): AWS signature version 4 (in headers and as
//! presigned URLs), objects in one piece or in parts, deletion and listing. Only what the cache
//! needs; works with any S3-compatible store (Hetzner, Scaleway, Cloudflare R2, Ceph …).

use std::time::Duration;

use bytes::Bytes;
use hmac::{Hmac, KeyInit, Mac};
use http_body_util::{BodyExt, Full};
use hyper::{HeaderMap, Method, Request, StatusCode};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use url::Url;

/// Parts of a large object (S3 allows 5 MiB to 5 GiB per part, at most 10 000 parts).
pub const PART_SIZE: u64 = 16 * 1024 * 1024;
/// Objects up to this size go up in one request.
pub const SINGLE_MAX: u64 = PART_SIZE;
const MAX_PARTS: u64 = 10_000;
/// Per request; a part of 16 MiB needs ~7 s at 20 Mbit/s.
const TIMEOUT: Duration = Duration::from_secs(300);
const ATTEMPTS: u32 = 3;
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, Debug)]
pub struct S3Config {
    /// E.g. `https://fsn1.your-objectstorage.com`.
    pub endpoint: Url,
    pub bucket: String,
    pub region: String,
    pub access_key: String,
    pub secret_key: String,
    /// `bucket.host/key` instead of `host/bucket/key`.
    pub virtual_host: bool,
}

impl S3Config {
    /// The origin objects are fetched from (for the web app's Content Security Policy).
    pub fn public_origin(&self) -> String {
        let (host, _) = self.host_and_path("");
        format!("{}://{host}", self.endpoint.scheme())
    }

    /// Host header and canonical (encoded) path of an object.
    fn host_and_path(&self, key: &str) -> (String, String) {
        let mut host = self.endpoint.host_str().unwrap_or_default().to_owned();
        if let Some(port) = self.endpoint.port() {
            host = format!("{host}:{port}");
        }
        let base = self.endpoint.path().trim_end_matches('/');
        if self.virtual_host {
            (
                format!("{}.{host}", self.bucket),
                format!("{base}/{}", encode(key, false)),
            )
        } else {
            (
                host,
                format!(
                    "{base}/{}/{}",
                    encode(&self.bucket, true),
                    encode(key, false)
                ),
            )
        }
    }
}

#[derive(Debug)]
pub enum Error {
    /// The store answered with an error.
    Status(u16, String),
    /// Not reachable, timed out, broken connection.
    Net(String),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Status(code, body) => write!(f, "S3 antwortet {code}: {body}"),
            Error::Net(m) => write!(f, "S3 nicht erreichbar: {m}"),
            Error::Io(m) => write!(f, "S3-Upload: {m}"),
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// URI encoding as SigV4 wants it: everything but `A-Z a-z 0-9 - _ . ~`; `/` kept in paths.
pub fn encode(s: &str, slash_too: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (b == b'/' && !slash_too)
        {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC nimmt jede Länge");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn sha256_hex(b: &[u8]) -> String {
    hex(&Sha256::digest(b))
}

fn amz_date(t: OffsetDateTime) -> (String, String) {
    let d = format!("{:04}{:02}{:02}", t.year(), u8::from(t.month()), t.day());
    let full = format!("{d}T{:02}{:02}{:02}Z", t.hour(), t.minute(), t.second());
    (d, full)
}

/// Signature version 4 for S3.
pub struct Signer<'a> {
    pub region: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

impl Signer<'_> {
    fn scope(&self, day: &str) -> String {
        format!("{day}/{}/s3/aws4_request", self.region)
    }

    fn signature(&self, day: &str, amz: &str, canonical: &str) -> String {
        let to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz}\n{}\n{}",
            self.scope(day),
            sha256_hex(canonical.as_bytes())
        );
        let k = hmac(
            format!("AWS4{}", self.secret_key).as_bytes(),
            day.as_bytes(),
        );
        let k = hmac(&k, self.region.as_bytes());
        let k = hmac(&k, b"s3");
        let k = hmac(&k, b"aws4_request");
        hex(&hmac(&k, to_sign.as_bytes()))
    }

    fn canonical_query(query: &[(String, String)]) -> String {
        let mut q: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| (encode(k, true), encode(v, true)))
            .collect();
        q.sort();
        q.iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Signs a request: adds `x-amz-date` and `x-amz-content-sha256` to `headers` (which must
    /// hold `host`) and returns the `Authorization` value. `path` is already encoded.
    pub fn sign(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        headers: &mut Vec<(String, String)>,
        payload_sha256: &str,
        now: OffsetDateTime,
    ) -> String {
        let (day, amz) = amz_date(now);
        headers.push(("x-amz-date".into(), amz.clone()));
        headers.push(("x-amz-content-sha256".into(), payload_sha256.into()));
        let mut h: Vec<(String, String)> = headers
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_owned()))
            .collect();
        h.sort();
        let signed: Vec<&str> = h.iter().map(|(k, _)| k.as_str()).collect();
        let signed = signed.join(";");
        let canonical = format!(
            "{method}\n{path}\n{}\n{}\n{signed}\n{payload_sha256}",
            Self::canonical_query(query),
            h.iter()
                .map(|(k, v)| format!("{k}:{v}\n"))
                .collect::<String>(),
        );
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{},SignedHeaders={signed},Signature={}",
            self.access_key,
            self.scope(&day),
            self.signature(&day, &amz, &canonical)
        )
    }

    /// The query string of a presigned URL (signed header: `host` only).
    pub fn presign(
        &self,
        method: &str,
        host: &str,
        path: &str,
        extra: &[(String, String)],
        expires: Duration,
        now: OffsetDateTime,
    ) -> String {
        let (day, amz) = amz_date(now);
        let mut query: Vec<(String, String)> = extra.to_vec();
        query.extend([
            ("X-Amz-Algorithm".into(), "AWS4-HMAC-SHA256".into()),
            (
                "X-Amz-Credential".into(),
                format!("{}/{}", self.access_key, self.scope(&day)),
            ),
            ("X-Amz-Date".into(), amz.clone()),
            ("X-Amz-Expires".into(), expires.as_secs().to_string()),
            ("X-Amz-SignedHeaders".into(), "host".into()),
        ]);
        let qs = Self::canonical_query(&query);
        let canonical = format!("{method}\n{path}\n{qs}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        format!(
            "{qs}&X-Amz-Signature={}",
            self.signature(&day, &amz, &canonical)
        )
    }
}

/// Text between `<name>` and `</name>` (first occurrence), unescaped.
fn tag(xml: &str, name: &str) -> Option<String> {
    tags(xml, name).next()
}

fn tags<'a>(xml: &'a str, name: &'a str) -> impl Iterator<Item = String> + 'a {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let mut rest = xml;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let end = rest[start..].find(&close)? + start;
        let v = &rest[start..end];
        rest = &rest[end + close.len()..];
        Some(
            v.replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        )
    })
}

/// Keeps uploads below a rate (bytes per second), so the home line stays usable.
pub struct Throttle {
    rate: Option<u64>,
    start: tokio::time::Instant,
    sent: u64,
}

impl Throttle {
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            rate: bytes_per_sec.filter(|r| *r > 0),
            start: tokio::time::Instant::now(),
            sent: 0,
        }
    }

    async fn sent(&mut self, n: u64) {
        self.sent += n;
        if let Some(rate) = self.rate {
            let due = Duration::from_secs_f64(self.sent as f64 / rate as f64);
            tokio::time::sleep_until(self.start + due).await;
        }
    }
}

pub struct S3 {
    pub cfg: S3Config,
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl S3 {
    pub fn new(cfg: S3Config) -> std::result::Result<Self, String> {
        match cfg.endpoint.scheme() {
            "https" => {}
            // Plain HTTP only to a store in the home network (tests, a local gateway).
            "http" => {
                let local = match cfg.endpoint.host() {
                    Some(url::Host::Ipv4(ip)) => crate::extract::tika::is_local(ip.into()),
                    Some(url::Host::Ipv6(ip)) => crate::extract::tika::is_local(ip.into()),
                    Some(url::Host::Domain(d)) => d == "localhost",
                    None => false,
                };
                if !local {
                    return Err("XLRX_S3_ENDPOINT: nur https://".into());
                }
            }
            _ => return Err("XLRX_S3_ENDPOINT: nur https://".into()),
        }
        let https = hyper_rustls::HttpsConnectorBuilder::new()
            .with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
            .map_err(|e| format!("TLS: {e}"))?
            .https_or_http()
            .enable_http1()
            .build();
        Ok(Self {
            cfg,
            client: Client::builder(TokioExecutor::new()).build(https),
        })
    }

    fn signer(&self) -> Signer<'_> {
        Signer {
            region: &self.cfg.region,
            access_key: &self.cfg.access_key,
            secret_key: &self.cfg.secret_key,
        }
    }

    async fn send(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        body: Bytes,
    ) -> Result<Answer> {
        let (host, path) = self.cfg.host_and_path(key);
        let payload = if body.is_empty() {
            EMPTY_SHA256.to_owned()
        } else {
            sha256_hex(&body)
        };
        let mut headers = vec![("host".to_owned(), host.clone())];
        let auth = self.signer().sign(
            method.as_str(),
            &path,
            query,
            &mut headers,
            &payload,
            OffsetDateTime::now_utc(),
        );
        let qs = query
            .iter()
            .map(|(k, v)| format!("{}={}", encode(k, true), encode(v, true)))
            .collect::<Vec<_>>()
            .join("&");
        let uri = format!(
            "{}://{host}{path}{}{qs}",
            self.cfg.endpoint.scheme(),
            if qs.is_empty() { "" } else { "?" }
        );
        let mut req = Request::builder().method(method).uri(&uri);
        for (k, v) in &headers {
            if k != "host" {
                req = req.header(k.as_str(), v.as_str());
            }
        }
        let req = req
            .header("authorization", auth)
            .header("content-length", body.len())
            .body(Full::new(body))
            .map_err(|e| Error::Net(e.to_string()))?;
        let res = tokio::time::timeout(TIMEOUT, self.client.request(req))
            .await
            .map_err(|_| Error::Net("Zeitüberschreitung".into()))?
            .map_err(|e| Error::Net(e.to_string()))?;
        let status = res.status();
        let headers = res.headers().clone();
        let body = tokio::time::timeout(TIMEOUT, res.into_body().collect())
            .await
            .map_err(|_| Error::Net("Zeitüberschreitung".into()))?
            .map_err(|e| Error::Net(e.to_string()))?
            .to_bytes();
        Ok(Answer {
            status,
            headers,
            body,
        })
    }

    /// Like [`send`], but only a 2xx answer counts; network errors and 5xx are tried again.
    async fn ok(
        &self,
        method: Method,
        key: &str,
        query: &[(String, String)],
        body: Bytes,
    ) -> Result<Answer> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let r = self.send(method.clone(), key, query, body.clone()).await;
            let retry = match &r {
                Ok(a) if a.status.is_success() => return r,
                Ok(a) => a.status.is_server_error() || a.status == StatusCode::TOO_MANY_REQUESTS,
                Err(_) => true,
            };
            if !retry || attempt >= ATTEMPTS {
                return match r {
                    Ok(a) => Err(Error::Status(
                        a.status.as_u16(),
                        String::from_utf8_lossy(&a.body).chars().take(300).collect(),
                    )),
                    Err(e) => Err(e),
                };
            }
            tokio::time::sleep(Duration::from_secs(2u64.pow(attempt))).await;
        }
    }

    pub async fn put(&self, key: &str, body: Bytes) -> Result<()> {
        self.ok(Method::PUT, key, &[], body).await.map(|_| ())
    }

    /// Uploads a file (one request up to 16 MiB, in parts above). The file must not change
    /// meanwhile: callers upload from a private snapshot.
    pub async fn put_file(
        &self,
        key: &str,
        path: &std::path::Path,
        throttle: &mut Throttle,
    ) -> Result<()> {
        use tokio::io::AsyncReadExt;
        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|e| Error::Io(e.to_string()))?;
        let size = file
            .metadata()
            .await
            .map_err(|e| Error::Io(e.to_string()))?
            .len();
        if size <= SINGLE_MAX {
            let mut buf = Vec::with_capacity(size as usize);
            file.read_to_end(&mut buf)
                .await
                .map_err(|e| Error::Io(e.to_string()))?;
            self.put(key, Bytes::from(buf)).await?;
            throttle.sent(size).await;
            return Ok(());
        }
        let part_size = PART_SIZE.max(size.div_ceil(MAX_PARTS - 1));
        let started = self
            .ok(
                Method::POST,
                key,
                &[("uploads".into(), String::new())],
                Bytes::new(),
            )
            .await?;
        let upload_id = tag(&String::from_utf8_lossy(&started.body), "UploadId")
            .ok_or_else(|| Error::Status(200, "keine UploadId".into()))?;
        let result = async {
            let mut etags = Vec::new();
            let mut number = 1u32;
            loop {
                let mut buf = Vec::with_capacity(part_size as usize);
                (&mut file)
                    .take(part_size)
                    .read_to_end(&mut buf)
                    .await
                    .map_err(|e| Error::Io(e.to_string()))?;
                if buf.is_empty() {
                    break;
                }
                let n = buf.len() as u64;
                let a = self
                    .ok(
                        Method::PUT,
                        key,
                        &[
                            ("partNumber".into(), number.to_string()),
                            ("uploadId".into(), upload_id.clone()),
                        ],
                        Bytes::from(buf),
                    )
                    .await?;
                let etag = a
                    .headers
                    .get("etag")
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| Error::Status(200, "kein ETag".into()))?
                    .to_owned();
                etags.push((number, etag));
                throttle.sent(n).await;
                number += 1;
            }
            let parts: String = etags
                .iter()
                .map(|(n, e)| {
                    let e = e.replace('&', "&amp;").replace('<', "&lt;");
                    format!("<Part><PartNumber>{n}</PartNumber><ETag>{e}</ETag></Part>")
                })
                .collect();
            let body = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
            let done = self
                .ok(
                    Method::POST,
                    key,
                    &[("uploadId".into(), upload_id.clone())],
                    Bytes::from(body),
                )
                .await?;
            // Completing can fail with status 200 and an error in the body.
            let text = String::from_utf8_lossy(&done.body);
            if text.contains("<Error>") {
                return Err(Error::Status(200, text.chars().take(300).collect()));
            }
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self
                .send(
                    Method::DELETE,
                    key,
                    &[("uploadId".into(), upload_id)],
                    Bytes::new(),
                )
                .await;
        }
        result
    }

    /// Deletes an object (also when it does not exist).
    pub async fn delete(&self, key: &str) -> Result<()> {
        match self.ok(Method::DELETE, key, &[], Bytes::new()).await {
            Err(Error::Status(404, _)) => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Size of an object, `None` if there is none.
    pub async fn size(&self, key: &str) -> Result<Option<u64>> {
        let a = self.send(Method::HEAD, key, &[], Bytes::new()).await?;
        match a.status {
            s if s.is_success() => Ok(a
                .headers
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())),
            StatusCode::NOT_FOUND => Ok(None),
            s => Err(Error::Status(s.as_u16(), String::new())),
        }
    }

    /// All keys with this prefix and their sizes.
    pub async fn list(&self, prefix: &str) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("list-type".to_owned(), "2".to_owned()),
                ("prefix".to_owned(), prefix.to_owned()),
            ];
            if let Some(t) = &token {
                q.push(("continuation-token".into(), t.clone()));
            }
            let a = self.ok(Method::GET, "", &q, Bytes::new()).await?;
            let xml = String::from_utf8_lossy(&a.body).into_owned();
            for c in tags(&xml, "Contents") {
                if let Some(k) = tag(&c, "Key") {
                    out.push((k, tag(&c, "Size").and_then(|s| s.parse().ok()).unwrap_or(0)));
                }
            }
            token = tag(&xml, "NextContinuationToken");
            if tag(&xml, "IsTruncated").as_deref() != Some("true") || token.is_none() {
                return Ok(out);
            }
        }
    }

    /// A URL to fetch an object without credentials for a while; the store answers with the
    /// given `Content-Disposition` and `Content-Type`.
    pub fn presign_get(
        &self,
        key: &str,
        expires: Duration,
        disposition: &str,
        content_type: &str,
    ) -> String {
        let (host, path) = self.cfg.host_and_path(key);
        let qs = self.signer().presign(
            "GET",
            &host,
            &path,
            &[
                ("response-content-disposition".into(), disposition.into()),
                ("response-content-type".into(), content_type.into()),
            ],
            expires,
            OffsetDateTime::now_utc(),
        );
        format!("{}://{host}{path}?{qs}", self.cfg.endpoint.scheme())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn signer() -> Signer<'static> {
        Signer {
            region: "us-east-1",
            access_key: AK,
            secret_key: SK,
        }
    }

    fn may_24_2013() -> OffsetDateTime {
        time::macros::datetime!(2013-05-24 00:00:00 UTC)
    }

    /// AWS documentation, "Signature Calculations for the Authorization Header: GET Object".
    #[test]
    fn signs_like_aws_get_object() {
        let mut headers = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            ("range".to_owned(), "bytes=0-9".to_owned()),
        ];
        let auth = signer().sign(
            "GET",
            "/test.txt",
            &[],
            &mut headers,
            EMPTY_SHA256,
            may_24_2013(),
        );
        assert_eq!(
            auth,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request,\
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date,\
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    /// AWS documentation, "Example: PUT Object" (with a `$` in the key).
    #[test]
    fn signs_like_aws_put_object() {
        let body = b"Welcome to Amazon S3.";
        let mut headers = vec![
            (
                "host".to_owned(),
                "examplebucket.s3.amazonaws.com".to_owned(),
            ),
            (
                "date".to_owned(),
                "Fri, 24 May 2013 00:00:00 GMT".to_owned(),
            ),
            (
                "x-amz-storage-class".to_owned(),
                "REDUCED_REDUNDANCY".to_owned(),
            ),
        ];
        let auth = signer().sign(
            "PUT",
            &format!("/{}", encode("test$file.text", false)),
            &[],
            &mut headers,
            &sha256_hex(body),
            may_24_2013(),
        );
        assert!(
            auth.ends_with(
                "Signature=98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd"
            ),
            "{auth}"
        );
    }

    /// AWS documentation, "Authenticating Requests: Using Query Parameters".
    #[test]
    fn presigns_like_aws() {
        let qs = signer().presign(
            "GET",
            "examplebucket.s3.amazonaws.com",
            "/test.txt",
            &[],
            Duration::from_secs(86400),
            may_24_2013(),
        );
        assert!(
            qs.ends_with(
                "X-Amz-Signature=aeeed9bbccd4d02ee5c0109b86d86835f995330da4c265957d157751f604d404"
            ),
            "{qs}"
        );
        assert!(qs.contains(
            "X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request"
        ));
    }

    #[test]
    fn addresses() {
        let mut cfg = S3Config {
            endpoint: "https://fsn1.your-objectstorage.com".parse().unwrap(),
            bucket: "xlrx-cache".into(),
            region: "fsn1".into(),
            access_key: AK.into(),
            secret_key: SK.into(),
            virtual_host: false,
        };
        assert_eq!(
            cfg.host_and_path("x/ab"),
            (
                "fsn1.your-objectstorage.com".into(),
                "/xlrx-cache/x/ab".into()
            )
        );
        assert_eq!(cfg.public_origin(), "https://fsn1.your-objectstorage.com");
        cfg.virtual_host = true;
        assert_eq!(
            cfg.host_and_path("x/ab"),
            (
                "xlrx-cache.fsn1.your-objectstorage.com".into(),
                "/x/ab".into()
            )
        );
        assert_eq!(
            cfg.public_origin(),
            "https://xlrx-cache.fsn1.your-objectstorage.com"
        );
        cfg.endpoint = "http://203.0.113.9:9000".parse().unwrap();
        assert!(S3::new(cfg.clone()).is_err(), "kein Klartext ins Internet");
        cfg.endpoint = "http://127.0.0.1:9000".parse().unwrap();
        assert!(S3::new(cfg).is_ok());
    }

    #[test]
    fn reads_xml() {
        let xml = "<R><Contents><Key>x/a&amp;b</Key><Size>3</Size></Contents>\
                   <Contents><Key>x/c</Key><Size>7</Size></Contents><IsTruncated>false</IsTruncated></R>";
        let keys: Vec<String> = tags(xml, "Contents")
            .filter_map(|c| tag(&c, "Key"))
            .collect();
        assert_eq!(keys, ["x/a&b", "x/c"]);
        assert_eq!(tag(xml, "IsTruncated").as_deref(), Some("false"));
    }
}

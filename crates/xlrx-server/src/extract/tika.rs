//! Apache Tika for Office, iWork, OpenDocument and mail (`PUT /tika/text`).
//!
//! Tika runs as a container on the NAS. Documents must never leave the home network (folders
//! marked "Nur lokal" may not be processed in the cloud), so only addresses there are accepted:
//! when configuring, and again for every address a name resolves to before connecting.

use std::net::{IpAddr, SocketAddr};
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper_util::rt::TokioIo;
use tokio::io::AsyncReadExt;
use url::{Host, Url};

/// Most text bytes taken from Tika.
const MAX_OUTPUT: usize = 8 * 1024 * 1024;
/// Parsing a large document on the NAS can take a while.
const TIME: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Debug)]
pub struct Tika {
    base: Url,
}

#[derive(Debug)]
pub enum Error {
    /// Tika cannot read this document (unknown format, encrypted, damaged).
    Rejected(u16),
    /// Not reachable, overloaded, time limit: worth another try later.
    Failed(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Rejected(code) => write!(f, "Tika lehnt das Dokument ab ({code})"),
            Error::Failed(m) => write!(f, "Tika: {m}"),
        }
    }
}

/// Addresses that stay in the home network: loopback, private ranges, link-local, unique local.
pub fn is_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_local(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            v6.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// Checks a configured Tika address: plain HTTP to an address in the home network, or to a name
/// without dots (a container on the same Docker network, e.g. `http://tika:9998`).
pub fn check_url(url: &Url) -> Result<(), String> {
    if url.scheme() != "http" {
        return Err("XLRX_TIKA_URL: nur http:// im Heimnetz (z. B. http://tika:9998)".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.query().is_some() {
        return Err("XLRX_TIKA_URL: ohne Zugangsdaten und Parameter angeben".into());
    }
    let local = match url.host() {
        Some(Host::Ipv4(ip)) => is_local(IpAddr::V4(ip)),
        Some(Host::Ipv6(ip)) => is_local(IpAddr::V6(ip)),
        Some(Host::Domain(name)) => name == "localhost" || !name.contains('.'),
        None => false,
    };
    if !local {
        return Err(format!(
            "XLRX_TIKA_URL: {url} liegt nicht im Heimnetz. Dokumente dürfen das NAS nicht verlassen."
        ));
    }
    Ok(())
}

impl Tika {
    pub fn new(url: Url) -> Result<Self, String> {
        check_url(&url)?;
        let mut base = url;
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        Ok(Self { base })
    }

    /// Resolves the host; refuses if any address lies outside the home network.
    async fn address(&self) -> Result<SocketAddr, Error> {
        let host = self.base.host_str().unwrap_or_default();
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let port = self.base.port_or_known_default().unwrap_or(80);
        let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| Error::Failed(format!("{host}: {e}")))?
            .collect();
        if addrs.is_empty() || addrs.iter().any(|a| !is_local(a.ip())) {
            return Err(Error::Failed(format!(
                "{host} löst nicht nur zu Adressen im Heimnetz auf – nichts gesendet"
            )));
        }
        Ok(addrs[0])
    }

    /// The plain text of a document. `ext` helps Tika tell the format (the name is not sent).
    pub async fn text(&self, file: &Path, ext: &str) -> Result<String, Error> {
        tokio::time::timeout(TIME, self.request(file, ext))
            .await
            .map_err(|_| Error::Failed(format!("Zeitlimit von {} s", TIME.as_secs())))?
    }

    async fn request(&self, file: &Path, ext: &str) -> Result<String, Error> {
        let failed = |e: &dyn std::fmt::Display| Error::Failed(e.to_string());
        let addr = self.address().await?;
        let stream = tokio::net::TcpStream::connect(addr)
            .await
            .map_err(|e| failed(&e))?;
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| failed(&e))?;
        let connection = tokio::spawn(conn);

        let f = tokio::fs::File::open(file).await.map_err(|e| failed(&e))?;
        let len = f.metadata().await.map_err(|e| failed(&e))?.len();
        let chunks = futures_util::stream::unfold(f, |mut f| async move {
            let mut buf = vec![0u8; 64 * 1024];
            match f.read(&mut buf).await {
                Ok(0) => None,
                Ok(n) => {
                    buf.truncate(n);
                    Some((Ok(Frame::data(Bytes::from(buf))), f))
                }
                Err(e) => Some((Err(e), f)),
            }
        });
        let url = self.base.join("tika/text").map_err(|e| failed(&e))?;
        let ext: String = ext
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .take(10)
            .collect();
        let req = hyper::Request::put(url.path())
            .header(
                hyper::header::HOST,
                self.base.host_str().unwrap_or_default(),
            )
            .header(hyper::header::ACCEPT, "text/plain")
            .header(hyper::header::CONTENT_LENGTH, len)
            .header(
                hyper::header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"dokument.{ext}\""),
            )
            .body(StreamBody::new(chunks.boxed()))
            .map_err(|e| failed(&e))?;
        let res = sender.send_request(req).await.map_err(|e| failed(&e))?;
        let status = res.status();
        if status.is_client_error() {
            connection.abort();
            return Err(Error::Rejected(status.as_u16()));
        }
        if !status.is_success() {
            connection.abort();
            return Err(Error::Failed(format!("Antwort {status}")));
        }
        let mut body = res.into_body();
        let mut out = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|e| failed(&e))?;
            if let Some(data) = frame.data_ref() {
                out.extend_from_slice(data);
                if out.len() >= MAX_OUTPUT {
                    out.truncate(MAX_OUTPUT);
                    break;
                }
            }
        }
        connection.abort();
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(u: &str) -> bool {
        check_url(&u.parse().unwrap()).is_ok()
    }

    #[test]
    fn only_addresses_in_the_home_network() {
        assert!(ok("http://tika:9998"));
        assert!(ok("http://localhost:9998"));
        assert!(ok("http://127.0.0.1:9998/"));
        assert!(ok("http://192.168.1.20:9998"));
        assert!(ok("http://10.0.0.5:9998"));
        assert!(ok("http://172.18.0.4:9998"));
        assert!(ok("http://[fd00::4]:9998"));
        assert!(ok("http://[::1]:9998"));
        assert!(!ok("http://tika.example.com:9998"));
        assert!(!ok("http://8.8.8.8:9998"));
        assert!(!ok("http://172.32.0.1:9998"));
        assert!(!ok("http://[2001:db8::1]:9998"));
        assert!(!ok("http://[::ffff:8.8.8.8]:9998"));
        assert!(!ok("https://tika:9998"));
        assert!(!ok("http://user:pw@tika:9998"));
    }
}

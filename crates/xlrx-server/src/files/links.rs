//! Public links (PLAN 9.2): a file or folder for anyone with the link, without an account.
//!
//! - The token carries 128 bits of randomness. The database keeps its SHA-256 for the lookup and
//!   the token itself only sealed with the server key, so whoever manages the item can copy the
//!   link again later; a copy of the database alone opens nothing.
//! - A link never grants more than its creator still has: if they lose access to the item, or
//!   their account is disabled, the link stops working until that changes.
//! - Optional password (argon2id, brute-force lock per link and per address), expiry and a
//!   maximum number of downloads. Removed links are gone at once.
//! - Nothing a link does can lose data: uploads never overwrite (a name in use gets a free one),
//!   new contents of a file keep the previous one as a version, and links cannot rename, move or
//!   delete anything.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, HeaderValue};
use base64::Engine as _;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use time::OffsetDateTime;

use super::access::{self, Role};
use super::db::{self, NodeRow, RootRow};
use crate::auth::tokens::random_bytes;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Ansehen: browse and preview in the browser.
    View,
    /// Herunterladen: also download.
    Download,
    /// Nur hochladen (Dateianfrage): add files to a folder without seeing what is in it.
    Upload,
    /// Bearbeiten: download, add files, and store new contents of files (the old ones stay as
    /// versions).
    Edit,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "view" => Some(Kind::View),
            "download" => Some(Kind::Download),
            "upload" => Some(Kind::Upload),
            "edit" => Some(Kind::Edit),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::View => "view",
            Kind::Download => "download",
            Kind::Upload => "upload",
            Kind::Edit => "edit",
        }
    }

    /// May see what is there (names, previews).
    pub fn browse(self) -> bool {
        self != Kind::Upload
    }

    pub fn download(self) -> bool {
        matches!(self, Kind::Download | Kind::Edit)
    }

    pub fn upload(self) -> bool {
        matches!(self, Kind::Upload | Kind::Edit)
    }

    pub fn replace(self) -> bool {
        self == Kind::Edit
    }

    /// The role the creator must still have on the item for the link to work.
    pub fn needs(self) -> Role {
        if self.upload() {
            Role::Editor
        } else {
            Role::Viewer
        }
    }
}

pub const LINK_COLS: &str = "id, token_hash, token_sealed, node_id, kind, password_hash, expires_at, \
                             max_downloads, downloads, created_by, created_at";

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct LinkRow {
    pub id: i64,
    pub token_hash: Vec<u8>,
    pub token_sealed: Vec<u8>,
    pub node_id: i64,
    pub kind: String,
    pub password_hash: Option<String>,
    pub expires_at: Option<OffsetDateTime>,
    pub max_downloads: Option<i32>,
    pub downloads: i32,
    pub created_by: i64,
    pub created_at: OffsetDateTime,
}

impl LinkRow {
    pub fn kind(&self) -> Kind {
        // An unknown value counts as the most restricted kind.
        Kind::parse(&self.kind).unwrap_or(Kind::View)
    }

    pub fn expired(&self) -> bool {
        self.expires_at
            .is_some_and(|t| t <= OffsetDateTime::now_utc())
    }
}

/// A new token (Base64url, 22 characters) and its hash.
pub fn new_token() -> (String, Vec<u8>) {
    let t = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes::<16>());
    let h = hash(&t);
    (t, h)
}

pub fn hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

/// Tokens look like this; anything else is not looked up at all.
pub fn well_formed(token: &str) -> bool {
    token.len() == 22
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn token_aad(token_hash: &[u8]) -> Vec<u8> {
    let mut aad = b"xlrx link v1 ".to_vec();
    aad.extend_from_slice(token_hash);
    aad
}

pub fn seal(st: &AppState, token: &str, token_hash: &[u8]) -> Vec<u8> {
    st.secrets.seal(&token_aad(token_hash), token.as_bytes())
}

/// The token of a link, for showing the link again.
pub fn token_of(st: &AppState, row: &LinkRow) -> Option<String> {
    let plain = st
        .secrets
        .open(&token_aad(&row.token_hash), &row.token_sealed)?;
    let token = String::from_utf8(plain).ok()?;
    // Bound to its row: a token sealed for another row never decrypts here.
    (hash(&token) == row.token_hash).then_some(token)
}

/// The address to hand out.
pub fn url(st: &AppState, token: &str) -> String {
    format!(
        "{}/s/{token}",
        st.cfg.public_url.as_str().trim_end_matches('/')
    )
}

pub async fn by_token(db: &PgPool, token: &str) -> ApiResult<Option<LinkRow>> {
    if !well_formed(token) {
        return Ok(None);
    }
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LINK_COLS} FROM links WHERE token_hash = $1"
    )))
    .bind(hash(token))
    .fetch_optional(db)
    .await?)
}

pub async fn by_id(db: &PgPool, id: i64) -> ApiResult<Option<LinkRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {LINK_COLS} FROM links WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

/// A link that works right now, with the item it points to.
pub struct Open {
    pub link: LinkRow,
    pub kind: Kind,
    pub node: NodeRow,
    pub root: RootRow,
}

/// The link behind a token if it may be used now: not removed, not expired, the item still
/// there, and its creator still allowed what the link allows.
pub async fn open(db: &PgPool, token: &str) -> ApiResult<Open> {
    let link = by_token(db, token).await?.ok_or(ApiError::NotFound)?;
    usable(db, link).await
}

/// A link if it may be used now (see [`open`]).
pub async fn usable(db: &PgPool, link: LinkRow) -> ApiResult<Open> {
    if link.expired() {
        return Err(ApiError::Gone("Dieser Link ist abgelaufen.".into()));
    }
    let kind = link.kind();
    let active: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND disabled_at IS NULL)",
    )
    .bind(link.created_by)
    .fetch_one(db)
    .await?;
    if !active {
        return Err(ApiError::NotFound);
    }
    let a = access::of(db, link.created_by, link.node_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if a.role < kind.needs() || a.node.deleted_at.is_some() {
        return Err(ApiError::NotFound);
    }
    Ok(Open {
        link,
        kind,
        node: a.node,
        root: a.root,
    })
}

/// A live node at or below the link's item, with the folders from the link's item down to it.
pub async fn within(db: &PgPool, top: i64, id: i64) -> ApiResult<(NodeRow, Vec<(i64, String)>)> {
    let node = db::node_by_id(db, id)
        .await?
        .filter(|n| n.deleted_at.is_none())
        .ok_or(ApiError::NotFound)?;
    let chain: Vec<(i64, String)> = sqlx::query_as(
        "WITH RECURSIVE up AS (
            SELECT id, parent_id, name, 0 AS depth FROM nodes WHERE id = $1
            UNION ALL
            SELECT n.id, n.parent_id, n.name, up.depth + 1
              FROM nodes n JOIN up ON n.id = up.parent_id
             WHERE up.depth < 1000 AND up.id <> $2
         )
         SELECT id, name FROM up ORDER BY depth DESC",
    )
    .bind(id)
    .bind(top)
    .fetch_all(db)
    .await?;
    if chain.first().map(|c| c.0) != Some(top) {
        return Err(ApiError::NotFound);
    }
    Ok((node, chain))
}

/// Counts a download. `false`: the allowed number is used up.
pub async fn count_download(db: &PgPool, link_id: i64) -> ApiResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "UPDATE links SET downloads = downloads + 1
          WHERE id = $1 AND (max_downloads IS NULL OR downloads < max_downloads)
          RETURNING id",
    )
    .bind(link_id)
    .fetch_optional(db)
    .await?;
    Ok(row.is_some())
}

// Unlocking a link with a password: a cookie only for this link's addresses, sealed with the
// server key and bound to the link and its current password.

pub const UNLOCK_COOKIE: &str = "xlrx_link";
const UNLOCK_HOURS: i64 = 12;

fn unlock_aad(link: &LinkRow) -> Vec<u8> {
    let mut aad = format!("xlrx link unlock v1 {} ", link.id).into_bytes();
    aad.extend_from_slice(&Sha256::digest(
        link.password_hash.as_deref().unwrap_or_default().as_bytes(),
    ));
    aad
}

pub fn unlock_cookie(st: &AppState, token: &str, link: &LinkRow) -> HeaderValue {
    let until = OffsetDateTime::now_utc().unix_timestamp() + UNLOCK_HOURS * 3600;
    let sealed = st.secrets.seal(&unlock_aad(link), &until.to_be_bytes());
    let value = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sealed);
    let secure = if st.cfg.secure_cookies() {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{UNLOCK_COOKIE}={value}; Path=/api/public/{token}; HttpOnly; SameSite=Strict; Max-Age={}{secure}",
        UNLOCK_HOURS * 3600
    ))
    .expect("gültiger Header")
}

/// Whether the request may use the link: no password, or unlocked by this browser.
pub fn unlocked(st: &AppState, link: &LinkRow, headers: &HeaderMap) -> bool {
    if link.password_hash.is_none() {
        return true;
    }
    let now = OffsetDateTime::now_utc().unix_timestamp();
    cookies(headers, UNLOCK_COOKIE).any(|v| {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(v)
            .ok()
            .and_then(|sealed| st.secrets.open(&unlock_aad(link), &sealed))
            .and_then(|plain| <[u8; 8]>::try_from(plain.as_slice()).ok())
            .is_some_and(|b| i64::from_be_bytes(b) > now)
    })
}

fn cookies<'a>(headers: &'a HeaderMap, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(move |c| {
            let (k, v) = c.trim().split_once('=')?;
            (k == name).then_some(v)
        })
}

/// Requests per address and minute on link pages (on top of the lockout after failures).
const PER_MINUTE: u32 = 600;

/// A simple fixed-window limit per address for everything reachable without an account.
#[derive(Default)]
pub struct RateLimit {
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimit {
    pub fn check(&self, ip: &str) -> ApiResult<()> {
        let now = Instant::now();
        let mut w = self.windows.lock().expect("mutex");
        if w.len() > 10_000 {
            w.retain(|_, (start, _)| now.duration_since(*start) < Duration::from_secs(60));
        }
        let e = w.entry(ip.to_owned()).or_insert((now, 0));
        if now.duration_since(e.0) >= Duration::from_secs(60) {
            *e = (now, 0);
        }
        e.1 += 1;
        if e.1 > PER_MINUTE {
            return Err(ApiError::TooManyRequests);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        let (t, h) = new_token();
        assert!(well_formed(&t), "{t}");
        assert_eq!(hash(&t), h);
        assert!(!well_formed("../../etc"));
        assert!(!well_formed(&format!("{t}x")));
        assert_ne!(new_token().0, t);
    }

    #[test]
    fn what_kinds_allow() {
        assert!(Kind::View.browse() && !Kind::View.download() && !Kind::View.upload());
        assert!(Kind::Download.download() && !Kind::Download.upload());
        assert!(!Kind::Upload.browse() && !Kind::Upload.download() && Kind::Upload.upload());
        assert!(!Kind::Upload.replace());
        assert!(Kind::Edit.download() && Kind::Edit.upload() && Kind::Edit.replace());
        assert_eq!(Kind::Upload.needs(), Role::Editor);
        assert_eq!(Kind::Download.needs(), Role::Viewer);
    }

    #[test]
    fn rate_limit_per_address() {
        let r = RateLimit::default();
        for _ in 0..PER_MINUTE {
            r.check("1.2.3.4").unwrap();
        }
        assert!(r.check("1.2.3.4").is_err());
        assert!(r.check("5.6.7.8").is_ok());
    }
}

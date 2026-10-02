//! Devices (Mac, iPhone): sign-in through the browser with PKCE, then rotating refresh tokens and
//! short-lived access tokens (PLAN 16.1).
//!
//! - The app opens `/device?…` in a browser session. After signing in (or confirming with a fresh
//!   second factor) the person allows the device, and the browser goes to `xlrx://auth?code=…`.
//! - The app exchanges the code together with its PKCE verifier for a pair of tokens.
//! - Refresh tokens rotate on every use. A refresh token presented again after its successor was
//!   used revokes the device: someone else has a copy. The one exception is a retry whose answer
//!   got lost: as long as the successor was never used, the previous token may be exchanged again
//!   (the unused successor is dropped).
//! - Every `device_confirm_days` days the device needs a second factor again.
//!
//! Only hashes of codes and tokens are stored.

use base64::Engine as _;
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, PgPool};
use time::{Duration, OffsetDateTime};
use uuid::Uuid;

use super::session::{ClientInfo, CurrentUser};
use super::throttle::{self, ip_key};
use super::tokens::{hash_token, new_token};
use crate::audit;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const ACCESS_TTL: Duration = Duration::minutes(15);
pub const CODE_TTL: Duration = Duration::minutes(2);
/// How long superseded grants and revoked devices are kept (reuse is recognized that long).
pub const KEEP: Duration = Duration::days(60);
/// The only places a code may be sent to: the app's own URL scheme.
pub const REDIRECT_URIS: &[&str] = &["xlrx://auth"];

/// What a device gets after signing in or refreshing.
#[derive(Serialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    /// Seconds until the access token expires.
    pub expires_in: i64,
    pub device_id: i64,
    /// From then on the device needs a second factor again.
    #[serde(with = "time::serde::rfc3339")]
    pub confirm_until: OffsetDateTime,
}

fn invalid_code() -> ApiError {
    ApiError::Unauthenticated(
        "code_invalid",
        "Der Anmeldecode ist ungültig oder abgelaufen. Bitte erneut anmelden.".into(),
    )
}

fn invalid_token() -> ApiError {
    ApiError::Unauthenticated(
        "token_invalid",
        "Der Zugang dieses Geräts ist ungültig oder abgelaufen.".into(),
    )
}

fn revoked() -> ApiError {
    ApiError::Unauthenticated(
        "revoked",
        "Dieses Gerät wurde abgemeldet. Bitte erneut anmelden.".into(),
    )
}

fn reauth_required() -> ApiError {
    ApiError::Unauthenticated(
        "reauth_required",
        "Bitte die Anmeldung auf diesem Gerät erneut bestätigen.".into(),
    )
}

/// A name shown in the device list ("MacBook von Anna").
pub fn check_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 100 || name.chars().any(char::is_control) {
        return Err(ApiError::bad("Ungültiger Gerätename."));
    }
    Ok(name.to_owned())
}

/// "macos", "ios", …
pub fn check_platform(platform: &str) -> ApiResult<String> {
    let ok = (1..=20).contains(&platform.len())
        && platform
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok {
        Ok(platform.to_owned())
    } else {
        Err(ApiError::bad("Ungültige Plattform."))
    }
}

/// PKCE with S256: the challenge is the Base64url SHA-256 of the verifier (43 characters).
pub fn check_challenge(challenge: &str) -> ApiResult<()> {
    let ok = challenge.len() == 43
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(challenge)
            .is_ok_and(|b| b.len() == 32);
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad("Ungültige PKCE-Challenge."))
    }
}

/// RFC 7636: 43–128 characters from the unreserved set, hashed it must give the challenge.
pub fn verifier_matches(verifier: &str, challenge: &str) -> bool {
    let well_formed = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b));
    well_formed
        && base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(Sha256::digest(verifier.as_bytes()))
            == challenge
}

pub struct CodeRequest {
    pub challenge: String,
    pub redirect_uri: String,
    pub name: String,
    pub platform: String,
}

/// A code for the browser to hand to the app (after the person allowed the device).
pub async fn create_code(db: &PgPool, user_id: i64, req: &CodeRequest) -> ApiResult<String> {
    if !REDIRECT_URIS.contains(&req.redirect_uri.as_str()) {
        return Err(ApiError::bad("Unbekannte Rücksprungadresse."));
    }
    check_challenge(&req.challenge)?;
    let (code, hash) = new_token();
    sqlx::query(
        "INSERT INTO device_codes (code_hash, user_id, challenge, redirect_uri, name, platform, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))",
    )
    .bind(hash)
    .bind(user_id)
    .bind(&req.challenge)
    .bind(&req.redirect_uri)
    .bind(check_name(&req.name)?)
    .bind(check_platform(&req.platform)?)
    .bind(CODE_TTL.whole_seconds() as f64)
    .execute(db)
    .await?;
    Ok(code)
}

/// New grant for a device; the previous one (if any) must already be superseded.
async fn issue(
    tx: &mut PgConnection,
    device_id: i64,
    parent: Option<i64>,
) -> ApiResult<(String, String)> {
    let (access, access_hash) = new_token();
    let (refresh, refresh_hash) = new_token();
    sqlx::query(
        "INSERT INTO device_grants (device_id, parent_id, refresh_hash, access_hash, access_expires_at)
         VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5))",
    )
    .bind(device_id)
    .bind(parent)
    .bind(refresh_hash)
    .bind(access_hash)
    .bind(ACCESS_TTL.whole_seconds() as f64)
    .execute(tx)
    .await?;
    Ok((access, refresh))
}

async fn tokens(
    tx: &mut PgConnection,
    st: &AppState,
    device_id: i64,
    parent: Option<i64>,
    confirmed_at: OffsetDateTime,
) -> ApiResult<Tokens> {
    let (access_token, refresh_token) = issue(tx, device_id, parent).await?;
    Ok(Tokens {
        access_token,
        refresh_token,
        expires_in: ACCESS_TTL.whole_seconds(),
        device_id,
        confirm_until: confirmed_at + Duration::days(st.cfg.device_confirm_days.into()),
    })
}

async fn supersede_all(tx: &mut PgConnection, device_id: i64) -> ApiResult<()> {
    sqlx::query(
        "UPDATE device_grants SET superseded_at = now() WHERE device_id = $1 AND superseded_at IS NULL",
    )
    .bind(device_id)
    .execute(tx)
    .await?;
    Ok(())
}

pub struct CodeExchange<'a> {
    pub code: &'a str,
    pub verifier: &'a str,
    pub redirect_uri: &'a str,
    /// The device's current refresh token, when it confirms again instead of signing in anew.
    pub previous: Option<&'a str>,
}

#[derive(sqlx::FromRow)]
struct CodeRow {
    user_id: i64,
    challenge: String,
    redirect_uri: String,
    name: String,
    platform: String,
    valid: bool,
}

/// Code and PKCE verifier → tokens. A code works once, even when the verifier is wrong.
pub async fn exchange_code(
    st: &AppState,
    req: CodeExchange<'_>,
    client: &ClientInfo,
) -> ApiResult<Tokens> {
    let ip = [ip_key(&client.ip)];
    throttle::check(&st.db, &ip).await?;
    let mut tx = st.db.begin().await?;
    let row: Option<CodeRow> = sqlx::query_as(
        "DELETE FROM device_codes c USING users u
          WHERE c.code_hash = $1 AND u.id = c.user_id
         RETURNING c.user_id, c.challenge, c.redirect_uri, c.name, c.platform,
                   c.expires_at > now() AND u.disabled_at IS NULL AS valid",
    )
    .bind(hash_token(req.code))
    .fetch_optional(&mut *tx)
    .await?;
    let ok = row.as_ref().is_some_and(|r| {
        r.valid
            && r.redirect_uri == req.redirect_uri
            && verifier_matches(req.verifier, &r.challenge)
    });
    let Some(row) = row.filter(|_| ok) else {
        // The code is used up either way.
        tx.commit().await?;
        throttle::fail(&st.db, &ip).await?;
        return Err(invalid_code());
    };

    // Confirming an existing device again: same person, still signed in on it. Its current grant
    // becomes the parent of the new one, so a retry with the old refresh token works as long as
    // the new tokens were never used (same rule as for refreshing).
    let existing: Option<(i64, i64)> = match req.previous {
        Some(prev) => {
            sqlx::query_as(
                "SELECT d.id, g.id FROM device_grants g JOIN devices d ON d.id = g.device_id
                  WHERE g.refresh_hash = $1 AND g.superseded_at IS NULL
                    AND d.user_id = $2 AND d.revoked_at IS NULL
                    FOR UPDATE OF d",
            )
            .bind(hash_token(prev))
            .bind(row.user_id)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => None,
    };
    let (device_id, parent, action) = match existing {
        Some((id, grant)) => {
            sqlx::query(
                "UPDATE devices SET name = $2, platform = $3, confirmed_at = now(), last_seen_at = now(),
                        last_ip = $4 WHERE id = $1",
            )
            .bind(id)
            .bind(&row.name)
            .bind(&row.platform)
            .bind(&client.ip)
            .execute(&mut *tx)
            .await?;
            supersede_all(&mut tx, id).await?;
            (id, Some(grant), "device_confirmed")
        }
        None => {
            let (id,): (i64,) = sqlx::query_as(
                "INSERT INTO devices (user_id, name, platform, last_ip) VALUES ($1, $2, $3, $4) RETURNING id",
            )
            .bind(row.user_id)
            .bind(&row.name)
            .bind(&row.platform)
            .bind(&client.ip)
            .fetch_one(&mut *tx)
            .await?;
            (id, None, "device_added")
        }
    };
    let tokens = tokens(&mut tx, st, device_id, parent, OffsetDateTime::now_utc()).await?;
    tx.commit().await?;
    audit::log(
        &st.db,
        Some(row.user_id),
        Some(row.user_id),
        action,
        Some(&client.ip),
        json!({"device": device_id, "name": row.name, "platform": row.platform}),
    )
    .await?;
    Ok(tokens)
}

#[derive(sqlx::FromRow)]
struct DeviceRow {
    user_id: i64,
    revoked: bool,
    confirmed_at: OffsetDateTime,
    disabled: bool,
}

/// Refresh token → new pair of tokens (rotation with reuse detection, see the module comment).
pub async fn refresh(st: &AppState, token: &str, client: &ClientInfo) -> ApiResult<Tokens> {
    let ip = [ip_key(&client.ip)];
    throttle::check(&st.db, &ip).await?;
    let hash = hash_token(token);
    let mut tx = st.db.begin().await?;
    let found: Option<(i64,)> =
        sqlx::query_as("SELECT device_id FROM device_grants WHERE refresh_hash = $1")
            .bind(&hash)
            .fetch_optional(&mut *tx)
            .await?;
    let Some((device_id,)) = found else {
        drop(tx);
        throttle::fail(&st.db, &ip).await?;
        return Err(invalid_token());
    };
    // One refresh per device at a time; everything below reads the state after the lock.
    let device: DeviceRow = sqlx::query_as(
        "SELECT d.user_id, d.revoked_at IS NOT NULL AS revoked, d.confirmed_at,
                u.disabled_at IS NOT NULL AS disabled
           FROM devices d JOIN users u ON u.id = d.user_id
          WHERE d.id = $1 FOR UPDATE OF d",
    )
    .bind(device_id)
    .fetch_one(&mut *tx)
    .await?;
    if device.revoked || device.disabled {
        return Err(revoked());
    }
    let confirm_until = device.confirmed_at + Duration::days(st.cfg.device_confirm_days.into());
    if confirm_until <= OffsetDateTime::now_utc() {
        return Err(reauth_required());
    }
    let (grant_id, superseded): (i64, bool) = sqlx::query_as(
        "SELECT id, superseded_at IS NOT NULL FROM device_grants WHERE refresh_hash = $1",
    )
    .bind(&hash)
    .fetch_one(&mut *tx)
    .await?;
    if superseded {
        let current: Option<(i64, Option<i64>, bool)> = sqlx::query_as(
            "SELECT id, parent_id, used_at IS NOT NULL FROM device_grants
              WHERE device_id = $1 AND superseded_at IS NULL",
        )
        .bind(device_id)
        .fetch_optional(&mut *tx)
        .await?;
        match current {
            // A retry: the answer with the successor never arrived.
            Some((id, Some(parent), false)) if parent == grant_id => {
                sqlx::query("UPDATE device_grants SET superseded_at = now() WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
            _ => {
                revoke_in(&mut tx, device_id, "token_reuse").await?;
                tx.commit().await?;
                tracing::warn!(
                    device = device_id,
                    "Gerätetoken erneut verwendet: Gerät abgemeldet"
                );
                audit::log(
                    &st.db,
                    None,
                    Some(device.user_id),
                    "device_token_reuse",
                    Some(&client.ip),
                    json!({"device": device_id}),
                )
                .await?;
                return Err(revoked());
            }
        }
    } else {
        sqlx::query(
            "UPDATE device_grants SET superseded_at = now(), used_at = coalesce(used_at, now())
              WHERE id = $1",
        )
        .bind(grant_id)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("UPDATE devices SET last_seen_at = now(), last_ip = $2 WHERE id = $1")
        .bind(device_id)
        .bind(&client.ip)
        .execute(&mut *tx)
        .await?;
    let tokens = tokens(&mut tx, st, device_id, Some(grant_id), device.confirmed_at).await?;
    tx.commit().await?;
    Ok(tokens)
}

#[derive(sqlx::FromRow)]
struct AccessRow {
    grant_id: i64,
    used: bool,
    device_id: i64,
    last_seen_at: OffsetDateTime,
    id: i64,
    uuid: Uuid,
    username: String,
    display_name: String,
    is_admin: bool,
}

/// The person behind an access token.
pub async fn authenticate(
    st: &AppState,
    token: &str,
    client: &ClientInfo,
) -> ApiResult<CurrentUser> {
    let row: Option<AccessRow> = sqlx::query_as(
        "SELECT g.id AS grant_id, g.used_at IS NOT NULL AS used, d.id AS device_id, d.last_seen_at,
                u.id, u.uuid, u.username, u.display_name, u.is_admin
           FROM device_grants g
           JOIN devices d ON d.id = g.device_id
           JOIN users u ON u.id = d.user_id
          WHERE g.access_hash = $1 AND g.superseded_at IS NULL AND g.access_expires_at > now()
            AND d.revoked_at IS NULL AND u.disabled_at IS NULL",
    )
    .bind(hash_token(token))
    .fetch_optional(&st.db)
    .await?;
    let row = row.ok_or_else(invalid_token)?;
    if !row.used {
        sqlx::query("UPDATE device_grants SET used_at = now() WHERE id = $1 AND used_at IS NULL")
            .bind(row.grant_id)
            .execute(&st.db)
            .await?;
    }
    if OffsetDateTime::now_utc() - row.last_seen_at > Duration::minutes(1) {
        sqlx::query("UPDATE devices SET last_seen_at = now(), last_ip = $2 WHERE id = $1")
            .bind(row.device_id)
            .bind(&client.ip)
            .execute(&st.db)
            .await?;
    }
    Ok(CurrentUser {
        id: row.id,
        uuid: row.uuid,
        username: row.username,
        display_name: row.display_name,
        is_admin: row.is_admin,
        session_id: None,
        device_id: Some(row.device_id),
        step_up_at: None,
    })
}

async fn revoke_in(tx: &mut PgConnection, device_id: i64, reason: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE devices SET revoked_at = now(), revoked_reason = $2 WHERE id = $1 AND revoked_at IS NULL",
    )
    .bind(device_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    supersede_all(tx, device_id).await
}

/// Signs a device out. `false` if it does not belong to the person or is already signed out.
pub async fn revoke(db: &PgPool, user_id: i64, device_id: i64, reason: &str) -> ApiResult<bool> {
    let mut tx = db.begin().await?;
    let found: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM devices WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL FOR UPDATE",
    )
    .bind(device_id)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await?;
    if found.is_none() {
        return Ok(false);
    }
    revoke_in(&mut tx, device_id, reason).await?;
    tx.commit().await?;
    Ok(true)
}

/// Signs out all devices of a person (second factors reset, account disabled).
pub async fn revoke_all(tx: &mut PgConnection, user_id: i64, reason: &str) -> ApiResult<()> {
    sqlx::query(
        "UPDATE device_grants SET superseded_at = now()
          WHERE superseded_at IS NULL AND device_id IN (SELECT id FROM devices WHERE user_id = $1)",
    )
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE devices SET revoked_at = now(), revoked_reason = $2 WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(user_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct DeviceInfo {
    pub id: i64,
    pub name: String,
    pub platform: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    pub last_ip: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub confirm_until: OffsetDateTime,
}

pub async fn list(st: &AppState, user_id: i64) -> ApiResult<Vec<DeviceInfo>> {
    Ok(sqlx::query_as(
        "SELECT id, name, platform, created_at, last_seen_at, last_ip,
                confirmed_at + make_interval(days => $2) AS confirm_until
           FROM devices WHERE user_id = $1 AND revoked_at IS NULL ORDER BY last_seen_at DESC",
    )
    .bind(user_id)
    .bind(st.cfg.device_confirm_days as i32)
    .fetch_all(&st.db)
    .await?)
}

/// Removes expired codes, and superseded grants and revoked devices past the retention time.
pub async fn cleanup(db: &PgPool) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM device_codes WHERE expires_at < now()")
        .execute(db)
        .await?;
    let keep = KEEP.whole_seconds() as f64;
    sqlx::query(
        "DELETE FROM device_grants WHERE superseded_at < now() - make_interval(secs => $1)",
    )
    .bind(keep)
    .execute(db)
    .await?;
    sqlx::query("DELETE FROM devices WHERE revoked_at < now() - make_interval(secs => $1)")
        .bind(keep)
        .execute(db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256() {
        // RFC 7636, appendix B.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
        check_challenge(challenge).unwrap();
        assert!(verifier_matches(verifier, challenge));
        assert!(!verifier_matches(&verifier.replace('d', "e"), challenge));
        assert!(!verifier_matches("kurz", challenge));
        assert!(check_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-c").is_err());
        assert!(check_challenge("E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw+cM").is_err());
    }

    #[test]
    fn names_and_platforms() {
        assert_eq!(
            check_name("  MacBook von Anna ").unwrap(),
            "MacBook von Anna"
        );
        assert!(check_name("").is_err());
        assert!(check_name("a\nb").is_err());
        assert!(check_platform("macos").is_ok());
        assert!(check_platform("macOS").is_err());
        assert!(check_platform("").is_err());
    }
}

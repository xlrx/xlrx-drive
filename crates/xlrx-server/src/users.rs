//! Accounts in the database.

use serde::Serialize;
use sqlx::{PgPool, Postgres, Transaction};
use time::OffsetDateTime;
use uuid::Uuid;
use webauthn_rs::prelude::Passkey;

use crate::auth::secret::totp_aad;
use crate::auth::tokens::{hash_recovery_code, new_recovery_code, new_token};
use crate::auth::totp;
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

pub const RECOVERY_CODES: usize = 10;
pub const INVITE_HOURS: i64 = 72;

#[derive(sqlx::FromRow, Clone, Debug)]
pub struct UserRow {
    pub id: i64,
    pub uuid: Uuid,
    pub username: String,
    pub username_folded: String,
    pub display_name: String,
    pub email: Option<String>,
    pub password_hash: Option<String>,
    pub totp_secret_enc: Option<Vec<u8>>,
    pub totp_last_step: Option<i64>,
    pub is_admin: bool,
    pub disabled_at: Option<OffsetDateTime>,
    pub created_at: OffsetDateTime,
}

const USER_COLS: &str = "id, uuid, username, username_folded, display_name, email, password_hash,
    totp_secret_enc, totp_last_step, is_admin, disabled_at, created_at";

/// Comparison form of a username.
pub fn fold(username: &str) -> String {
    username.trim().to_lowercase()
}

pub fn validate_username(u: &str) -> Result<(), String> {
    let n = u.chars().count();
    if !(2..=64).contains(&n) {
        return Err("Der Benutzername muss 2 bis 64 Zeichen lang sein.".into());
    }
    if !u
        .chars()
        .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err("Erlaubt sind Buchstaben, Ziffern, „.“, „_“ und „-“.".into());
    }
    Ok(())
}

pub async fn by_folded(db: &PgPool, folded: &str) -> ApiResult<Option<UserRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLS} FROM users WHERE username_folded = $1"
    )))
    .bind(folded)
    .fetch_optional(db)
    .await?)
}

pub async fn by_id(db: &PgPool, id: i64) -> ApiResult<Option<UserRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLS} FROM users WHERE id = $1"
    )))
    .bind(id)
    .fetch_optional(db)
    .await?)
}

pub async fn by_id_required(db: &PgPool, id: i64) -> ApiResult<UserRow> {
    by_id(db, id).await?.ok_or(ApiError::NotFound)
}

pub async fn all(db: &PgPool) -> ApiResult<Vec<UserRow>> {
    Ok(sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {USER_COLS} FROM users ORDER BY username_folded"
    )))
    .fetch_all(db)
    .await?)
}

pub async fn create(
    db: &PgPool,
    username: &str,
    display_name: &str,
    email: Option<&str>,
    is_admin: bool,
) -> ApiResult<UserRow> {
    let username = username.trim();
    validate_username(username).map_err(ApiError::BadRequest)?;
    let display_name = display_name.trim();
    if display_name.is_empty() || display_name.chars().count() > 100 {
        return Err(ApiError::bad(
            "Der Anzeigename muss 1 bis 100 Zeichen lang sein.",
        ));
    }
    let res = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "INSERT INTO users (username, username_folded, display_name, email, is_admin)
         VALUES ($1, $2, $3, $4, $5) RETURNING {USER_COLS}"
    )))
    .bind(username)
    .bind(fold(username))
    .bind(display_name)
    .bind(email.map(str::trim).filter(|e| !e.is_empty()))
    .bind(is_admin)
    .fetch_one(db)
    .await;
    match res {
        Ok(u) => Ok(u),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(ApiError::Conflict(
            "Diesen Benutzernamen gibt es schon.".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// New setup link (72 h). The person's earlier, unused links expire.
pub async fn create_invite(
    db: &PgPool,
    user_id: i64,
    created_by: Option<i64>,
) -> ApiResult<String> {
    let (token, hash) = new_token();
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM invites WHERE user_id = $1 AND used_at IS NULL")
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO invites (token_hash, user_id, created_by, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(hours => $4))",
    )
    .bind(hash)
    .bind(user_id)
    .bind(created_by)
    .bind(INVITE_HOURS as i32)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(token)
}

pub fn setup_url(state: &AppState, token: &str) -> String {
    let base = state.cfg.public_url.as_str().trim_end_matches('/');
    // In the fragment: this way the token never shows up in any server or proxy log.
    format!("{base}/setup#{token}")
}

/// Replaces all recovery codes and returns the new ones (visible only this one time).
pub async fn replace_recovery_codes(
    tx: &mut Transaction<'_, Postgres>,
    user_id: i64,
) -> ApiResult<Vec<String>> {
    sqlx::query("DELETE FROM recovery_codes WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut **tx)
        .await?;
    let codes: Vec<String> = (0..RECOVERY_CODES).map(|_| new_recovery_code()).collect();
    for c in &codes {
        sqlx::query("INSERT INTO recovery_codes (user_id, code_hash) VALUES ($1, $2)")
            .bind(user_id)
            .bind(hash_recovery_code(&c.replace('-', "")))
            .execute(&mut **tx)
            .await?;
    }
    Ok(codes)
}

pub async fn recovery_left(db: &PgPool, user_id: i64) -> ApiResult<i64> {
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(*) FROM recovery_codes WHERE user_id = $1 AND used_at IS NULL",
    )
    .bind(user_id)
    .fetch_one(db)
    .await?;
    Ok(n)
}

/// Redeems a recovery code (valid only once).
pub async fn consume_recovery_code(db: &PgPool, user_id: i64, input: &str) -> ApiResult<bool> {
    let Some(norm) = crate::auth::tokens::normalize_recovery_code(input) else {
        return Ok(false);
    };
    let r = sqlx::query(
        "UPDATE recovery_codes SET used_at = now()
          WHERE user_id = $1 AND code_hash = $2 AND used_at IS NULL",
    )
    .bind(user_id)
    .bind(hash_recovery_code(&norm))
    .execute(db)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Verifies a TOTP code of the account with replay protection (atomically in the DB).
pub async fn verify_totp(state: &AppState, user: &UserRow, code: &str) -> ApiResult<bool> {
    let Some(enc) = &user.totp_secret_enc else {
        return Ok(false);
    };
    let secret = state.secrets.open(&totp_aad(user.id), enc).ok_or_else(|| {
        ApiError::Internal("TOTP-Geheimnis nicht lesbar (falscher Schlüssel?)".into())
    })?;
    let Some(step) = totp::verify(&secret, code, unix_now(), user.totp_last_step) else {
        return Ok(false);
    };
    let r = sqlx::query(
        "UPDATE users SET totp_last_step = $2
          WHERE id = $1 AND (totp_last_step IS NULL OR totp_last_step < $2)",
    )
    .bind(user.id)
    .bind(step)
    .execute(&state.db)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(sqlx::FromRow)]
pub struct PasskeyRow {
    pub id: i64,
    pub name: String,
    pub passkey: sqlx::types::Json<Passkey>,
    pub created_at: OffsetDateTime,
    pub last_used_at: Option<OffsetDateTime>,
}

pub async fn passkeys(db: &PgPool, user_id: i64) -> ApiResult<Vec<PasskeyRow>> {
    Ok(sqlx::query_as(
        "SELECT id, name, passkey, created_at, last_used_at FROM passkeys WHERE user_id = $1 ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(db)
    .await?)
}

pub async fn add_passkey(db: &PgPool, user_id: i64, name: &str, pk: &Passkey) -> ApiResult<()> {
    let name = name.trim();
    let name = if name.is_empty() { "Passkey" } else { name };
    let res = sqlx::query(
        "INSERT INTO passkeys (user_id, name, credential_id, passkey) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(name.chars().take(60).collect::<String>())
    .bind(pk.cred_id().as_ref().to_vec())
    .bind(sqlx::types::Json(pk))
    .execute(db)
    .await;
    match res {
        Ok(_) => Ok(()),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(ApiError::Conflict(
            "Dieser Passkey ist schon registriert.".into(),
        )),
        Err(e) => Err(e.into()),
    }
}

/// Public view of an account.
#[derive(Serialize)]
pub struct Me {
    pub id: i64,
    pub username: String,
    pub display_name: String,
    pub email: Option<String>,
    pub is_admin: bool,
    pub totp: bool,
    pub passkeys: Vec<PasskeyInfo>,
    pub recovery_codes_left: i64,
}

#[derive(Serialize)]
pub struct PasskeyInfo {
    pub id: i64,
    pub name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_used_at: Option<OffsetDateTime>,
}

pub async fn me(db: &PgPool, user_id: i64) -> ApiResult<Me> {
    let u = by_id_required(db, user_id).await?;
    let pks = passkeys(db, user_id).await?;
    Ok(Me {
        id: u.id,
        username: u.username,
        display_name: u.display_name,
        email: u.email,
        is_admin: u.is_admin,
        totp: u.totp_secret_enc.is_some(),
        passkeys: pks
            .into_iter()
            .map(|p| PasskeyInfo {
                id: p.id,
                name: p.name,
                created_at: p.created_at,
                last_used_at: p.last_used_at,
            })
            .collect(),
        recovery_codes_left: recovery_left(db, user_id).await?,
    })
}

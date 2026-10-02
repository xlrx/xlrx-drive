//! Devices (Mac, iPhone): allow one in the browser, exchange its code or refresh token for access
//! tokens, list and sign out (PLAN 16.1, flow in [`crate::auth::device`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;

use crate::audit;
use crate::auth::device::{self, CodeExchange, CodeRequest, DeviceInfo, Tokens};
use crate::auth::session::{ClientInfo, CurrentUser};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;

#[derive(Deserialize)]
pub struct AuthorizeReq {
    /// PKCE S256 challenge of the app.
    pub challenge: String,
    pub redirect_uri: String,
    /// Handed back to the app unchanged.
    pub state: String,
    pub name: String,
    pub platform: String,
}

#[derive(Serialize)]
pub struct AuthorizeResp {
    /// Where the browser goes next: back to the app, with the code.
    pub redirect: String,
}

/// The signed-in person allows a device. Adding a device needs a fresh second factor.
pub async fn authorize(
    State(st): State<AppState>,
    me: CurrentUser,
    Json(req): Json<AuthorizeReq>,
) -> ApiResult<Json<AuthorizeResp>> {
    me.session()?;
    me.require_step_up()?;
    if req.state.len() > 500 {
        return Err(ApiError::bad("Ungültiger Zustand."));
    }
    let code = device::create_code(
        &st.db,
        me.id,
        &CodeRequest {
            challenge: req.challenge,
            redirect_uri: req.redirect_uri.clone(),
            name: req.name,
            platform: req.platform,
        },
    )
    .await?;
    let mut url = Url::parse(&req.redirect_uri)
        .map_err(|_| ApiError::bad("Unbekannte Rücksprungadresse."))?;
    url.query_pairs_mut()
        .append_pair("code", &code)
        .append_pair("state", &req.state);
    Ok(Json(AuthorizeResp {
        redirect: url.into(),
    }))
}

#[derive(Deserialize)]
#[serde(tag = "grant_type", rename_all = "snake_case")]
pub enum TokenReq {
    AuthorizationCode {
        code: String,
        code_verifier: String,
        redirect_uri: String,
        /// Set when an existing device confirms again: its current refresh token.
        refresh_token: Option<String>,
    },
    RefreshToken {
        refresh_token: String,
    },
}

/// Code (with PKCE verifier) or refresh token → tokens. Called by the app, without a cookie.
pub async fn token(
    State(st): State<AppState>,
    client: ClientInfo,
    Json(req): Json<TokenReq>,
) -> ApiResult<Json<Tokens>> {
    let tokens = match &req {
        TokenReq::AuthorizationCode {
            code,
            code_verifier,
            redirect_uri,
            refresh_token,
        } => {
            device::exchange_code(
                &st,
                CodeExchange {
                    code,
                    verifier: code_verifier,
                    redirect_uri,
                    previous: refresh_token.as_deref(),
                },
                &client,
            )
            .await?
        }
        TokenReq::RefreshToken { refresh_token } => {
            device::refresh(&st, refresh_token, &client).await?
        }
    };
    Ok(Json(tokens))
}

/// The device signs itself out.
pub async fn logout(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
) -> ApiResult<StatusCode> {
    let id = me
        .device_id
        .ok_or_else(|| ApiError::bad("Nur für Geräte."))?;
    if device::revoke(&st.db, me.id, id, "logout").await? {
        audit::log(
            &st.db,
            Some(me.id),
            Some(me.id),
            "device_logout",
            Some(&client.ip),
            json!({"device": id}),
        )
        .await?;
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list(State(st): State<AppState>, me: CurrentUser) -> ApiResult<Json<Vec<DeviceInfo>>> {
    Ok(Json(device::list(&st, me.id).await?))
}

pub async fn revoke(
    State(st): State<AppState>,
    me: CurrentUser,
    client: ClientInfo,
    Path(id): Path<i64>,
) -> ApiResult<StatusCode> {
    if !device::revoke(&st.db, me.id, id, "user").await? {
        return Err(ApiError::NotFound);
    }
    audit::log(
        &st.db,
        Some(me.id),
        Some(me.id),
        "device_revoked",
        Some(&client.ip),
        json!({"device": id}),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

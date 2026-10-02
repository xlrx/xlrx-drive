//! HTTP interface: `/api/…` (JSON), `/healthz`, and – if configured – the web app.

pub mod admin;
pub mod auth;
pub mod files;
pub mod me;
pub mod setup;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, ORIGIN, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
    X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::error::ApiError;
use crate::state::AppState;
use crate::web;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        // Sign-in
        .route("/auth/login", post(auth::login))
        .route("/auth/totp", post(auth::totp))
        .route("/auth/recovery", post(auth::recovery))
        .route("/auth/passkey/begin", post(auth::passkey_begin))
        .route("/auth/passkey/finish", post(auth::passkey_finish))
        .route("/auth/logout", post(auth::logout))
        .route("/auth/step-up/totp", post(auth::step_up_totp))
        .route(
            "/auth/step-up/passkey/begin",
            post(auth::step_up_passkey_begin),
        )
        .route(
            "/auth/step-up/passkey/finish",
            post(auth::step_up_passkey_finish),
        )
        // Setup via invite link
        .route("/setup/start", post(setup::start))
        .route("/setup/password", post(setup::password))
        .route("/setup/totp/begin", post(setup::totp_begin))
        .route("/setup/totp/confirm", post(setup::totp_confirm))
        .route("/setup/passkey/begin", post(setup::passkey_begin))
        .route("/setup/passkey/finish", post(setup::passkey_finish))
        // Own account
        .route("/me", get(me::get_me))
        .route("/me/password", post(me::change_password))
        .route("/me/totp", delete(me::totp_remove))
        .route("/me/totp/begin", post(me::totp_begin))
        .route("/me/totp/confirm", post(me::totp_confirm))
        .route("/me/passkeys/begin", post(me::passkey_begin))
        .route("/me/passkeys/finish", post(me::passkey_finish))
        .route(
            "/me/passkeys/{id}",
            patch(me::passkey_rename).delete(me::passkey_remove),
        )
        .route("/me/recovery-codes", post(me::recovery_regenerate))
        .route("/me/sessions", get(me::sessions))
        .route("/me/sessions/{id}", delete(me::session_revoke))
        // Files
        .route("/roots", get(files::list_roots))
        .route("/roots/{id}/scan", post(files::scan_root))
        .route("/nodes/{id}", get(files::get_node))
        .route("/nodes/{id}/children", get(files::children))
        .route("/nodes/{id}/content", get(files::content))
        // Administration
        .route(
            "/admin/users",
            get(admin::list_users).post(admin::create_user),
        )
        .route("/admin/users/{id}/invite", post(admin::invite))
        .route(
            "/admin/users/{id}/reset-factors",
            post(admin::reset_factors),
        )
        .route("/admin/users/{id}/disabled", post(admin::set_disabled))
        .route("/admin/audit", get(admin::audit_log))
        .fallback(|| async { ApiError::NotFound })
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        .layer(SetResponseHeaderLayer::overriding(
            CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        // File content sets a stricter policy of its own (sandbox).
        .layer(SetResponseHeaderLayer::if_not_present(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(web::MINIMAL_CSP),
        ));

    let mut app = Router::new()
        .route("/healthz", get(health))
        .nest("/api", api)
        .with_state(state.clone());
    if let Some(dir) = &state.cfg.web_dir {
        // Single-page app: unknown paths serve index.html.
        let index = ServeFile::new(dir.join("index.html"));
        app = app.fallback_service(ServeDir::new(dir).fallback(index));
    }
    // Strict CSP for the web app: only its own scripts plus the hashed inline scripts of the
    // generated pages. API responses keep the minimal policy set above.
    let csp = state.web_csp.as_deref().unwrap_or(web::MINIMAL_CSP);
    let csp = HeaderValue::from_str(csp).expect("CSP is a valid header value");
    // Security headers for all responses.
    app.layer(SetResponseHeaderLayer::if_not_present(
        CONTENT_SECURITY_POLICY,
        csp,
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        X_FRAME_OPTIONS,
        HeaderValue::from_static("DENY"),
    ))
    .layer(SetResponseHeaderLayer::if_not_present(
        HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    ))
    .layer(TraceLayer::new_for_http())
}

/// CSRF protection: state-changing requests only from our own origin (besides `SameSite=Strict`).
async fn check_origin(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return next.run(req).await;
    }
    let ok = req
        .headers()
        .get(ORIGIN)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|o| st.cfg.allowed_origins().iter().any(|a| a == o));
    if ok {
        next.run(req).await
    } else {
        ApiError::forbidden("Ungültige Herkunft der Anfrage.").into_response()
    }
}

async fn health(State(st): State<AppState>) -> Response {
    match sqlx::query("SELECT 1").execute(&st.db).await {
        Ok(_) => (StatusCode::OK, "ok").into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "Datenbank nicht erreichbar");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "Datenbank nicht erreichbar",
            )
                .into_response()
        }
    }
}

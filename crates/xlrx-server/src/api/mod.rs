//! HTTP interface: `/api/…` (JSON), `/healthz`, and – if configured – the web app.

pub mod activity;
pub mod admin;
pub mod auth;
pub mod devices;
pub mod files;
pub mod links;
pub mod me;
pub mod notifications;
pub mod search;
pub mod setup;
pub mod shares;
pub mod stars;
pub mod suggest;
pub mod sync;
pub mod uploads;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::header::{
    AUTHORIZATION, CACHE_CONTROL, CONTENT_SECURITY_POLICY, ORIGIN, REFERRER_POLICY,
    X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::{HeaderName, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post, put};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::error::ApiError;
use crate::state::AppState;
use crate::web;

pub fn router(state: AppState) -> Router {
    // Browser only: sign-in, the account itself, administration. Device tokens are refused here.
    let browser = Router::new()
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
        .route("/me/password", post(me::change_password))
        .route("/me/totp", delete(me::totp_remove))
        .route("/me/prefetch", put(me::set_prefetch))
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
        .route("/me/devices", get(devices::list))
        .route("/me/devices/{id}", delete(devices::revoke))
        .route("/devices/authorize", post(devices::authorize))
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
        .route("/admin/search", get(admin::search_status))
        .route(
            "/admin/groups",
            get(admin::list_groups).post(admin::create_group),
        )
        .route(
            "/admin/groups/{id}",
            put(admin::update_group).delete(admin::delete_group),
        )
        .route(
            "/admin/spaces",
            get(admin::list_spaces).post(admin::create_space),
        )
        .route("/admin/spaces/{id}", put(admin::update_space))
        .route("/admin/data-classes", get(admin::data_classes))
        .route("/admin/cache", get(admin::cache_status))
        .route("/admin/cache/clear", post(admin::cache_clear))
        .route("/admin/ai", get(admin::ai_status).post(admin::ai_action))
        .route("/admin/ai/budget", put(admin::ai_budget))
        .route("/admin/jobs/retry", post(admin::retry_jobs))
        .layer(middleware::from_fn(browser_only));

    // Browser and devices: files and sync.
    let shared = Router::new()
        .route("/me", get(me::get_me))
        .route("/devices/logout", post(devices::logout))
        // Files
        .route("/roots", get(files::list_roots))
        .route("/roots/{id}/scan", post(files::scan_root))
        .route("/roots/{id}/trash", get(files::trash_list))
        .route(
            "/nodes/{id}",
            get(files::get_node)
                .patch(files::update_node)
                .delete(files::delete_node),
        )
        .route("/nodes/{id}/children", get(files::children))
        .route("/nodes/{id}/data-class", put(files::set_data_class))
        .route("/nodes/{id}/thumbnail", get(files::thumbnail))
        .route("/recent", get(files::recent))
        .route("/activity", get(activity::activity))
        .route("/suggestions", get(suggest::suggestions))
        .route("/notifications", get(notifications::list))
        .route("/starred", get(stars::list))
        .route("/nodes/{id}/star", put(stars::star).delete(stars::unstar))
        .route("/notifications/read", post(notifications::read))
        .route("/suggestions/opened", post(suggest::suggestion_opened))
        .route("/nodes/{id}/opened", post(suggest::opened))
        .route("/people", get(shares::people))
        .route("/shared", get(shares::shared_with_me))
        .route(
            "/nodes/{id}/shares",
            get(shares::node_access).post(shares::create),
        )
        .route("/shares/{id}", patch(shares::update).delete(shares::remove))
        .route("/nodes/{id}/links", get(links::list).post(links::create))
        .route("/links/{id}", delete(links::remove))
        .route("/search", get(search::search))
        .route("/search/suggest", get(search::suggest))
        .route(
            "/nodes/{id}/content",
            get(files::content).put(files::replace_content),
        )
        .route("/nodes/{id}/files", post(files::upload))
        .route("/uploads", post(uploads::create))
        .route("/uploads/{id}", get(uploads::status).delete(uploads::abort))
        .route("/uploads/{id}/parts", put(uploads::put_part))
        .route("/uploads/{id}/commit", post(uploads::commit))
        .route("/nodes/{id}/folders", post(files::create_folder))
        .route("/nodes/{id}/versions", get(files::versions))
        .route("/versions/{id}/content", get(files::version_content))
        .route("/versions/{id}/restore", post(files::restore_version))
        .route("/trash/{id}/restore", post(files::restore))
        .route("/trash/{id}", delete(files::purge))
        // Sync
        .route("/sync/changes", get(sync::changes))
        .route("/sync/notify", get(sync::notify))
        .route("/sync/ops", post(sync::op))
        .route("/sync/ops/{device}/{op_id}", get(sync::op_result))
        .route(
            "/sync/content/{hash}",
            get(sync::content_available).put(sync::put_content),
        );

    // Public links: no account, no cookie of a session; rate-limited and locked out per address.
    let public = Router::new()
        .route("/public/{token}", get(links::public_info))
        .route("/public/{token}/unlock", post(links::unlock))
        .route("/public/{token}/nodes/{id}", get(links::public_node))
        .route(
            "/public/{token}/nodes/{id}/children",
            get(links::public_children),
        )
        .route(
            "/public/{token}/nodes/{id}/content",
            get(links::public_content).put(links::public_replace),
        )
        .route(
            "/public/{token}/nodes/{id}/thumbnail",
            get(links::public_thumbnail),
        )
        .route(
            "/public/{token}/nodes/{id}/files",
            post(links::public_upload),
        );

    let api = browser
        .merge(shared)
        .merge(public)
        .fallback(|| async { ApiError::NotFound })
        .layer(middleware::from_fn_with_state(state.clone(), check_origin))
        // Called by apps (no browser, no cookie): proves itself with a PKCE verifier or a
        // refresh token, so it needs no origin check.
        .route("/devices/token", post(devices::token))
        // Nothing is kept unless a response says otherwise (thumbnails of a known revision).
        .layer(SetResponseHeaderLayer::if_not_present(
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
/// Requests with a device token are judged by the token alone, never by a cookie, and a browser
/// does not add such a header on its own – no forgery possible.
async fn check_origin(State(st): State<AppState>, req: Request, next: Next) -> Response {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS)
        || req.headers().contains_key(AUTHORIZATION)
    {
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

/// Device tokens are not accepted for sign-in, the account itself or administration.
async fn browser_only(req: Request, next: Next) -> Response {
    if req.headers().contains_key(AUTHORIZATION) {
        ApiError::forbidden("Nur im Browser möglich.").into_response()
    } else {
        next.run(req).await
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

//! Serving the web app: SPA fallback and a strict CSP that allows exactly the inline scripts of
//! the generated pages.

mod common;

use axum::body::Body;
use axum::http::{Request, header};
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;
use xlrx_server::AppState;

#[tokio::test]
async fn spa_with_hashed_inline_scripts() {
    let Some(db) = TestDb::new().await else {
        return;
    };
    let dir = std::env::temp_dir().join(format!("xlrx-web-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("_nuxt")).unwrap();
    std::fs::write(
        dir.join("index.html"),
        r#"<html><head><script>window.__NUXT__={};</script><script type="module" src="/_nuxt/a.js"></script></head><body><div id="__nuxt"></div></body></html>"#,
    )
    .unwrap();
    std::fs::write(dir.join("_nuxt/a.js"), "console.log(1)").unwrap();
    let mut cfg = config();
    cfg.web_dir = Some(dir.clone());
    let app = xlrx_server::router(AppState::new(db.pool.clone(), cfg).unwrap());

    let get = |path: &str| {
        let app = app.clone();
        let req = Request::get(path).body(Body::empty()).unwrap();
        async move { app.oneshot(req).await.unwrap() }
    };

    // Client-side routes get index.html with a CSP that allows only its inline script by hash.
    for path in ["/", "/setup", "/settings/security"] {
        let res = get(path).await;
        assert_eq!(res.status(), 200, "{path}");
        let csp = res.headers()[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(csp.contains("script-src 'self' 'sha256-"), "{csp}");
        assert!(
            !csp.contains("unsafe-inline'; img")
                || csp.contains("style-src 'self' 'unsafe-inline'")
        );
        assert!(!csp.contains("script-src 'self' 'unsafe-inline'"), "{csp}");
        let body = res.into_body().collect().await.unwrap().to_bytes();
        assert!(String::from_utf8_lossy(&body).contains("__nuxt"));
    }
    let js = get("/_nuxt/a.js").await;
    assert_eq!(js.status(), 200);

    // The API keeps its own minimal policy and JSON errors.
    let api = get("/api/gibtsnicht").await;
    assert_eq!(api.status(), 404);
    assert_eq!(
        api.headers()[header::CONTENT_SECURITY_POLICY],
        xlrx_server::web::MINIMAL_CSP
    );
    std::fs::remove_dir_all(dir).unwrap();
    db.drop_db().await;
}

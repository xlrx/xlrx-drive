//! Devices (Mac, iPhone): allowing one in the browser, PKCE code exchange, access with a device
//! token, rotating refresh tokens with reuse detection, confirming again, signing out.

mod common;

use base64::Engine as _;
use common::files::*;
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

/// A PKCE verifier and its S256 challenge.
fn pkce(seed: &str) -> (String, String) {
    let seed: String = seed
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let verifier = format!("verifier-{seed}-0123456789abcdefghijklmnopqrstuvwxyz");
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// The browser allows a device; returns the code from the redirect back to the app.
async fn authorize(web: &mut Client, challenge: &str, name: &str) -> String {
    let r = web
        .post(
            "/api/devices/authorize",
            json!({"challenge": challenge, "redirect_uri": "xlrx://auth", "state": "zustand 1",
                   "name": name, "platform": "macos"}),
        )
        .await;
    let redirect = url::Url::parse(r.ok()["redirect"].as_str().unwrap()).unwrap();
    assert_eq!(
        (redirect.scheme(), redirect.host_str()),
        ("xlrx", Some("auth"))
    );
    let q: std::collections::HashMap<_, _> = redirect.query_pairs().into_owned().collect();
    assert_eq!(q["state"], "zustand 1");
    q["code"].clone()
}

async fn exchange(app: &mut Client, code: &str, verifier: &str, previous: Option<&str>) -> Resp {
    app.post(
        "/api/devices/token",
        json!({"grant_type": "authorization_code", "code": code, "code_verifier": verifier,
               "redirect_uri": "xlrx://auth", "refresh_token": previous}),
    )
    .await
}

async fn refresh(app: &mut Client, token: &str) -> Resp {
    app.post(
        "/api/devices/token",
        json!({"grant_type": "refresh_token", "refresh_token": token}),
    )
    .await
}

/// Signs a new device in for the person of `web`; returns the app client (with access token) and
/// the tokens.
async fn new_device(env: &Env, web: &mut Client, name: &str) -> (Client, Value) {
    let (verifier, challenge) = pkce(name);
    let code = authorize(web, &challenge, name).await;
    let mut app = env.device();
    let tokens = exchange(&mut app, &code, &verifier, None)
        .await
        .ok()
        .clone();
    app.bearer = Some(tokens["access_token"].as_str().unwrap().to_owned());
    (app, tokens)
}

fn reason(r: &Resp) -> &str {
    r.body["reason"].as_str().unwrap_or_default()
}

fn s(v: &Value, key: &str) -> String {
    v[key].as_str().unwrap().to_owned()
}

async fn user_id(env: &Env, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(username)
        .fetch_one(&env.state.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn geraet_anmelden_und_zugreifen() {
    let env = env_or_skip!();
    let (mut web, root_id, root_node, _dir) = signed_in(&env, "anna").await;
    let (verifier, challenge) = pkce("mac");
    let code = authorize(&mut web, &challenge, "MacBook von Anna").await;

    // The app exchanges the code – without cookie and origin.
    let mut app = env.device();
    let tokens = exchange(&mut app, &code, &verifier, None)
        .await
        .ok()
        .clone();
    assert_eq!(tokens["expires_in"], 900);
    assert!(tokens["confirm_until"].as_str().unwrap().starts_with("20"));
    // A code works once.
    let again = exchange(&mut app, &code, &verifier, None).await;
    assert_eq!(
        (again.status.as_u16(), reason(&again)),
        (401, "code_invalid")
    );

    // Files and sync with the access token, state-changing requests included.
    app.bearer = Some(s(&tokens, "access_token"));
    let roots = app.get("/api/roots").await;
    assert_eq!(roots.ok()[0]["id"], root_id);
    app.post(
        &format!("/api/nodes/{root_node}/folders"),
        json!({"name": "Vom Mac"}),
    )
    .await
    .ok();
    let changes = app
        .get(&format!("/api/sync/changes?root={root_id}&cursor=0"))
        .await;
    assert_eq!(changes.ok()["changes"][0]["state"]["name"], "Vom Mac");
    assert_eq!(app.get("/api/me").await.ok()["username"], "anna");

    // The account itself only in the browser.
    for (method, path, body) in [
        (
            "POST",
            "/api/me/password",
            Some(json!({"password": "ein ganz neues Passwort"})),
        ),
        ("GET", "/api/me/sessions", None),
        ("GET", "/api/me/devices", None),
        ("GET", "/api/admin/users", None),
        ("POST", "/api/auth/logout", None),
        (
            "POST",
            "/api/devices/authorize",
            Some(
                json!({"challenge": challenge, "redirect_uri": "xlrx://auth", "state": "",
                        "name": "x", "platform": "ios"}),
            ),
        ),
    ] {
        let r = app.send(method, path, body).await;
        assert_eq!(r.status, 403, "{method} {path}: {}", r.body);
    }
    // With a token, the cookie does not count: a wrong token is not rescued by a valid session.
    let mut both = web.clone_with_bearer("falsch");
    let r = both.get("/api/roots").await;
    assert_eq!((r.status.as_u16(), reason(&r)), (401, "token_invalid"));

    // The browser shows the device.
    let list = web.get("/api/me/devices").await.ok().clone();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(
        (list[0]["name"].as_str(), list[0]["platform"].as_str()),
        (Some("MacBook von Anna"), Some("macos"))
    );
    assert_eq!(list[0]["id"], tokens["device_id"]);

    // Wrong verifier: refused, and the code is used up.
    let (v2, c2) = pkce("zweites");
    let code = authorize(&mut web, &c2, "iPhone").await;
    let r = exchange(&mut env.device(), &code, &verifier, None).await;
    assert_eq!(reason(&r), "code_invalid");
    let r = exchange(&mut env.device(), &code, &v2, None).await;
    assert_eq!(reason(&r), "code_invalid");
    // Wrong return address.
    let code = authorize(&mut web, &c2, "iPhone").await;
    let r = env
        .device()
        .post(
            "/api/devices/token",
            json!({"grant_type": "authorization_code", "code": code, "code_verifier": v2,
                   "redirect_uri": "xlrx://anders"}),
        )
        .await;
    assert_eq!(reason(&r), "code_invalid");

    // Only the app's own address, only proper challenges.
    for (redirect, ch) in [
        ("https://boese.example/cb", c2.as_str()),
        ("xlrx://auth", "zu-kurz"),
    ] {
        let r = web
            .post(
                "/api/devices/authorize",
                json!({"challenge": ch, "redirect_uri": redirect, "state": "",
                       "name": "x", "platform": "ios"}),
            )
            .await;
        assert_eq!(r.status, 400, "{redirect} {ch}");
    }

    // Allowing a device needs a fresh second factor.
    sqlx::query("UPDATE sessions SET step_up_at = now() - interval '1 hour'")
        .execute(&env.state.db)
        .await
        .unwrap();
    let r = web
        .post(
            "/api/devices/authorize",
            json!({"challenge": c2, "redirect_uri": "xlrx://auth", "state": "",
                   "name": "x", "platform": "ios"}),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".into())
    );

    // Someone else sees nothing and cannot sign the device out.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(bert.get("/api/me/devices").await.ok(), &json!([]));
    let r = bert
        .send(
            "DELETE",
            &format!("/api/me/devices/{}", tokens["device_id"]),
            None,
        )
        .await;
    assert_eq!(r.status, 404);
    assert_eq!(app.get("/api/roots").await.status, 200);
    env.finish().await;
}

#[tokio::test]
async fn refresh_rotiert_und_erkennt_wiederverwendung() {
    let env = env_or_skip!();
    let (mut web, _, _, _) = signed_in(&env, "carla").await;
    let (mut app, t0) = new_device(&env, &mut web, "Mac").await;
    let device = t0["device_id"].clone();

    // Rotation: new pair, the old access token stops working.
    let t1 = refresh(&mut app, &s(&t0, "refresh_token"))
        .await
        .ok()
        .clone();
    assert_eq!(t1["device_id"], device);
    assert_ne!(t1["refresh_token"], t0["refresh_token"]);
    let r = app.get("/api/roots").await;
    assert_eq!((r.status.as_u16(), reason(&r)), (401, "token_invalid"));
    app.bearer = Some(s(&t1, "access_token"));
    assert_eq!(app.get("/api/roots").await.status, 200);

    // A retry whose answer got lost: the previous token works again while the new pair is unused.
    let lost = refresh(&mut app, &s(&t1, "refresh_token"))
        .await
        .ok()
        .clone();
    let t3 = refresh(&mut app, &s(&t1, "refresh_token"))
        .await
        .ok()
        .clone();
    app.bearer = Some(s(&lost, "access_token"));
    assert_eq!(app.get("/api/roots").await.status, 401, "verworfenes Paar");
    app.bearer = Some(s(&t3, "access_token"));
    assert_eq!(app.get("/api/roots").await.status, 200);

    // The new pair was used: the old refresh token again means a copy exists – device signed out.
    let r = refresh(&mut app, &s(&t1, "refresh_token")).await;
    assert_eq!((r.status.as_u16(), reason(&r)), (401, "revoked"));
    let r = app.get("/api/roots").await;
    assert_eq!(r.status, 401);
    let r = refresh(&mut app, &s(&t3, "refresh_token")).await;
    assert_eq!(reason(&r), "revoked");
    assert_eq!(web.get("/api/me/devices").await.ok(), &json!([]));
    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE action = 'device_token_reuse' AND details->>'device' = $1",
    )
    .bind(device.to_string())
    .fetch_one(&env.state.db)
    .await
    .unwrap();
    assert_eq!(logged, 1);

    // The discarded pair of a retry is never valid again either: presenting it signs out.
    let (mut app, t0) = new_device(&env, &mut web, "iPhone").await;
    let lost = refresh(&mut app, &s(&t0, "refresh_token"))
        .await
        .ok()
        .clone();
    let t2 = refresh(&mut app, &s(&t0, "refresh_token"))
        .await
        .ok()
        .clone();
    let r = refresh(&mut app, &s(&lost, "refresh_token")).await;
    assert_eq!(reason(&r), "revoked");
    let r = refresh(&mut app, &s(&t2, "refresh_token")).await;
    assert_eq!(reason(&r), "revoked");

    // Unknown tokens.
    let r = refresh(&mut app, "gibt-es-nicht").await;
    assert_eq!((r.status.as_u16(), reason(&r)), (401, "token_invalid"));
    env.finish().await;
}

#[tokio::test]
async fn bestaetigung_laeuft_ab_und_wird_erneuert() {
    let env = env_or_skip!();
    let (mut web, _, _, _) = signed_in(&env, "dora").await;
    let (mut app, t0) = new_device(&env, &mut web, "Mac").await;
    let device = t0["device_id"].clone();
    sqlx::query("UPDATE devices SET confirmed_at = now() - interval '31 days'")
        .execute(&env.state.db)
        .await
        .unwrap();

    // Refreshing stops, but the device stays (no reuse, just a confirmation due).
    let r = refresh(&mut app, &s(&t0, "refresh_token")).await;
    assert_eq!((r.status.as_u16(), reason(&r)), (401, "reauth_required"));
    assert_eq!(web.get("/api/me/devices").await.ok()[0]["id"], device);

    // Confirming again through the browser keeps the device.
    let (verifier, challenge) = pkce("erneut");
    let code = authorize(&mut web, &challenge, "Mac (neu)").await;
    let t1 = exchange(&mut app, &code, &verifier, Some(&s(&t0, "refresh_token")))
        .await
        .ok()
        .clone();
    assert_eq!(t1["device_id"], device);
    let list = web.get("/api/me/devices").await.ok().clone();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "Mac (neu)");
    let t2 = refresh(&mut app, &s(&t1, "refresh_token"))
        .await
        .ok()
        .clone();
    app.bearer = Some(s(&t2, "access_token"));
    assert_eq!(app.get("/api/roots").await.status, 200);

    // The refresh token from before the confirmation, presented after the new one was used.
    let r = refresh(&mut app, &s(&t0, "refresh_token")).await;
    assert_eq!(reason(&r), "revoked");

    // Someone else's refresh token does not turn a code into a confirmation of that device.
    let (mut app, t0) = new_device(&env, &mut web, "Mac 2").await;
    let (mut emil, _, _, _) = signed_in(&env, "emil").await;
    let (verifier, challenge) = pkce("fremd");
    let code = authorize(&mut emil, &challenge, "Emils Mac").await;
    let t = exchange(
        &mut env.device(),
        &code,
        &verifier,
        Some(&s(&t0, "refresh_token")),
    )
    .await
    .ok()
    .clone();
    assert_ne!(t["device_id"], t0["device_id"]);
    assert_eq!(app.get("/api/roots").await.status, 200);
    env.finish().await;
}

#[tokio::test]
async fn abmelden_widerrufen_und_zuruecksetzen() {
    let env = env_or_skip!();
    let (mut web, _, _, _) = signed_in(&env, "fritz").await;

    // The app signs itself out.
    let (mut app, t) = new_device(&env, &mut web, "Mac").await;
    assert_eq!(app.post("/api/devices/logout", json!({})).await.status, 204);
    assert_eq!(app.get("/api/roots").await.status, 401);
    assert_eq!(
        reason(&refresh(&mut app, &s(&t, "refresh_token")).await),
        "revoked"
    );
    // Only devices sign out this way.
    assert_eq!(web.post("/api/devices/logout", json!({})).await.status, 400);

    // Signed out from the browser.
    let (mut app, t) = new_device(&env, &mut web, "iPhone").await;
    let r = web
        .send(
            "DELETE",
            &format!("/api/me/devices/{}", t["device_id"]),
            None,
        )
        .await;
    assert_eq!(r.status, 204);
    assert_eq!(app.get("/api/roots").await.status, 401);
    assert_eq!(
        reason(&refresh(&mut app, &s(&t, "refresh_token")).await),
        "revoked"
    );

    // Second factors reset by an admin: all devices out.
    let (mut app, t) = new_device(&env, &mut web, "iPad").await;
    xlrx_server::api::admin::reset_user_factors(&env.state.db, user_id(&env, "fritz").await)
        .await
        .unwrap();
    assert_eq!(app.get("/api/roots").await.status, 401);
    assert_eq!(
        reason(&refresh(&mut app, &s(&t, "refresh_token")).await),
        "revoked"
    );
    env.finish().await;
}

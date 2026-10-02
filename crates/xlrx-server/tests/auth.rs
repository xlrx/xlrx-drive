//! Anmeldung, Einrichtung, Step-up und Verwaltung über die HTTP-Schnittstelle, gegen echtes PostgreSQL.

mod common;

use common::*;
use serde_json::json;
use url::Url;
use webauthn_authenticator_rs::WebauthnAuthenticator;
use webauthn_authenticator_rs::softpasskey::SoftPasskey;
use webauthn_rs::prelude::{CreationChallengeResponse, RequestChallengeResponse};
use xlrx_server::auth::totp::code_at_step;

macro_rules! env_or_skip {
    () => {
        match Env::new().await {
            Some(e) => e,
            None => return,
        }
    };
}

async fn last_step(env: &Env, username: &str) -> u64 {
    let (s,): (Option<i64>,) =
        sqlx::query_as("SELECT totp_last_step FROM users WHERE username = $1")
            .bind(username)
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    s.unwrap() as u64
}

/// Erlaubt im Test einen weiteren Code im selben 30-s-Fenster (sonst greift der Replay-Schutz).
async fn allow_next_totp(env: &Env, username: &str) {
    sqlx::query("UPDATE users SET totp_last_step = $2 WHERE username = $1")
        .bind(username)
        .bind(now_step() as i64 - 2)
        .execute(&env.db.pool)
        .await
        .unwrap();
}

async fn login_totp(env: &Env, username: &str, secret: &[u8]) -> Client {
    allow_next_totp(env, username).await;
    let mut c = env.client();
    let r = c
        .post(
            "/api/auth/login",
            json!({ "username": username, "password": PASSWORD }),
        )
        .await;
    let ch = r.ok()["challenge"].as_str().unwrap().to_owned();
    let code = code_at_step(secret, now_step());
    c.post("/api/auth/totp", json!({ "challenge": ch, "code": code }))
        .await
        .ok();
    c
}

#[tokio::test]
async fn einrichtung_und_anmeldung_mit_totp() {
    let env = env_or_skip!();
    let invite = env.invite("anna", false).await;
    let (mut c, secret, codes) = setup_with_totp(&env, &invite).await;
    assert_eq!(codes.len(), 10);
    let me = c.get("/api/me").await;
    assert_eq!(me.ok()["username"], "anna");
    assert_eq!(me.body["totp"], true);
    assert_eq!(me.body["recovery_codes_left"], 10);

    assert_eq!(c.post("/api/auth/logout", json!({})).await.status, 204);
    assert!(c.cookie.is_none());
    assert_eq!(c.get("/api/me").await.status, 401);

    // Falsches Passwort und unbekanntes Konto: gleiche Antwort.
    let wrong = c
        .post(
            "/api/auth/login",
            json!({"username": "anna", "password": "falsch falsch falsch"}),
        )
        .await;
    let unknown = c
        .post(
            "/api/auth/login",
            json!({"username": "niemand", "password": PASSWORD}),
        )
        .await;
    assert_eq!(wrong.status, 401);
    assert_eq!((unknown.status, unknown.err()), (wrong.status, wrong.err()));

    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "Anna", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.ok()["totp"], true);
    assert_eq!(r.body["passkey"], false);
    assert!(c.cookie.is_none(), "Passwort allein meldet nicht an");
    let ch = r.body["challenge"].as_str().unwrap().to_owned();

    // Der Code aus der Einrichtung gilt nicht noch einmal.
    let used = last_step(&env, "anna").await;
    let replay = c
        .post(
            "/api/auth/totp",
            json!({"challenge": ch, "code": code_at_step(&secret, used)}),
        )
        .await;
    assert_eq!(replay.status, 401);
    let ok = c
        .post(
            "/api/auth/totp",
            json!({"challenge": ch, "code": code_at_step(&secret, used + 1)}),
        )
        .await;
    assert_eq!(ok.ok()["username"], "anna");
    assert_eq!(c.get("/api/me").await.status, 200);

    // Ein verbrauchter Zwischenschritt gilt nicht noch einmal.
    let again = c
        .post("/api/auth/totp", json!({"challenge": ch, "code": "000000"}))
        .await;
    assert_eq!(again.status, 401);
    env.finish().await;
}

#[tokio::test]
async fn einladung_gilt_nur_einmal_und_passwort_regeln() {
    let env = env_or_skip!();
    let invite = env.invite("bert", false).await;
    let mut c = env.client();
    let t = c
        .post("/api/setup/start", json!({"invite": invite}))
        .await
        .ok()["setup_token"]
        .as_str()
        .unwrap()
        .to_owned();
    for weak in ["kurz", "passwort1234", "bert-ist-toll-2026"] {
        let r = c
            .post(
                "/api/setup/password",
                json!({"setup_token": t, "password": weak}),
            )
            .await;
        assert_eq!(r.status, 400, "{weak}: {}", r.body);
    }
    // Ohne Passwort kein zweiter Faktor.
    c.post("/api/setup/totp/begin", json!({"setup_token": t}))
        .await
        .ok();
    let r = c
        .post(
            "/api/setup/totp/confirm",
            json!({"setup_token": t, "code": "123456"}),
        )
        .await;
    assert_eq!(r.status, 400);

    let (_c2, _, _) = setup_with_totp(&env, &invite).await;
    let r = env
        .client()
        .post("/api/setup/start", json!({"invite": invite}))
        .await;
    assert_eq!(r.status, 401, "Link verbraucht");
    env.finish().await;
}

async fn password_step(env: &Env, username: &str) -> (Client, String) {
    let mut c = env.client();
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": username, "password": PASSWORD}),
        )
        .await;
    let ch = r.ok()["challenge"].as_str().unwrap().to_owned();
    (c, ch)
}

#[tokio::test]
async fn wiederherstellungscodes_gelten_je_einmal() {
    let env = env_or_skip!();
    let invite = env.invite("carla", false).await;
    let (_, _, codes) = setup_with_totp(&env, &invite).await;
    let (mut c1, ch) = password_step(&env, "carla").await;
    c1.post(
        "/api/auth/recovery",
        json!({"challenge": ch, "code": codes[0]}),
    )
    .await
    .ok();
    let (mut c2, ch) = password_step(&env, "carla").await;
    let r = c2
        .post(
            "/api/auth/recovery",
            json!({"challenge": ch, "code": codes[0]}),
        )
        .await;
    assert_eq!(r.status, 401, "schon verbraucht");
    let sloppy = codes[1].to_lowercase().replace('-', " ");
    c2.post(
        "/api/auth/recovery",
        json!({"challenge": ch, "code": sloppy}),
    )
    .await
    .ok();
    assert_eq!(c2.get("/api/me").await.ok()["recovery_codes_left"], 8);
    env.finish().await;
}

#[tokio::test]
async fn sperre_nach_fehlversuchen() {
    let env = env_or_skip!();
    let invite = env.invite("dora", false).await;
    setup_with_totp(&env, &invite).await;
    let mut c = env.client();
    for _ in 0..5 {
        let r = c
            .post(
                "/api/auth/login",
                json!({"username": "dora", "password": "falsch falsch falsch"}),
            )
            .await;
        assert_eq!(r.status, 401);
    }
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "dora", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.status, 429, "gesperrt, auch mit richtigem Passwort");
    // Unbekannte Konten werden genauso gesperrt (keine Unterscheidung von außen).
    for _ in 0..5 {
        c.post(
            "/api/auth/login",
            json!({"username": "gibtsnicht", "password": "falsch falsch falsch"}),
        )
        .await;
    }
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "gibtsnicht", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.status, 429);
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM audit_log WHERE action = 'login_locked'")
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert!(n >= 1);
    env.finish().await;
}

#[tokio::test]
async fn zweiter_faktor_versuche_begrenzt() {
    let env = env_or_skip!();
    let invite = env.invite("emil", false).await;
    let (_, secret, _) = setup_with_totp(&env, &invite).await;
    allow_next_totp(&env, "emil").await;
    let mut c = env.client();
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "emil", "password": PASSWORD}),
        )
        .await;
    let ch = r.ok()["challenge"].as_str().unwrap().to_owned();
    for _ in 0..5 {
        let r = c
            .post("/api/auth/totp", json!({"challenge": ch, "code": "000000"}))
            .await;
        assert!(r.status == 401 || r.status == 429);
    }
    let code = code_at_step(&secret, now_step());
    let r = c
        .post("/api/auth/totp", json!({"challenge": ch, "code": code}))
        .await;
    assert!(
        !r.status.is_success(),
        "nach 5 Fehlversuchen ist der Zwischenschritt verbraucht"
    );
    env.finish().await;
}

#[tokio::test]
async fn nur_eigene_origin_darf_aendern() {
    let env = env_or_skip!();
    let mut c = env.client();
    c.origin = None;
    let r = c
        .post("/api/auth/login", json!({"username": "x", "password": "y"}))
        .await;
    assert_eq!(r.status, 403);
    c.origin = Some("https://boese.example".into());
    let r = c
        .post("/api/auth/login", json!({"username": "x", "password": "y"}))
        .await;
    assert_eq!(r.status, 403);
    assert_eq!(c.get("/api/me").await.status, 401);
    assert_eq!(c.get("/api/gibtsnicht").await.status, 404);
    assert_eq!(c.get("/healthz").await.status, 200);
    env.finish().await;
}

#[tokio::test]
async fn admin_aktionen_brauchen_step_up() {
    let env = env_or_skip!();
    let invite = env.invite("admin", true).await;
    let (mut a, secret, _) = setup_with_totp(&env, &invite).await;
    // Die Anmeldung zählt als frischer zweiter Faktor – hier künstlich veralten lassen.
    sqlx::query("UPDATE sessions SET step_up_at = now() - interval '1 hour'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let new_user = json!({"username": "fritz", "display_name": "Fritz"});
    let r = a.post("/api/admin/users", new_user.clone()).await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".into())
    );
    allow_next_totp(&env, "admin").await;
    let r = a
        .post(
            "/api/auth/step-up/totp",
            json!({"code": code_at_step(&secret, now_step())}),
        )
        .await;
    assert_eq!(r.status, 204);
    let r = a.post("/api/admin/users", new_user).await;
    let url = r.ok()["setup_url"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with("https://drive.example.test/setup#"),
        "{url}"
    );
    let token = url.split('#').nth(1).unwrap();
    let (mut f, _, _) = setup_with_totp(&env, token).await;
    assert_eq!(f.get("/api/me").await.ok()["username"], "fritz");
    // Normale Konten dürfen nicht verwalten.
    assert_eq!(f.get("/api/admin/users").await.status, 403);
    let users = a.get("/api/admin/users").await;
    assert_eq!(users.ok().as_array().unwrap().len(), 2);
    let audit = a.get("/api/admin/audit").await;
    assert!(
        audit
            .ok()
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["action"] == "user_created")
    );
    env.finish().await;
}

#[tokio::test]
async fn admin_setzt_faktoren_zurueck_und_sperrt() {
    let env = env_or_skip!();
    let (mut a, _, _) = setup_with_totp(&env, &env.invite("admin", true).await).await;
    let (mut g, _, _) = setup_with_totp(&env, &env.invite("gerda", false).await).await;
    let users = a.get("/api/admin/users").await;
    let gid = users
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "gerda")
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    let r = a
        .post(&format!("/api/admin/users/{gid}/reset-factors"), json!({}))
        .await;
    let url = r.ok()["setup_url"].as_str().unwrap().to_owned();
    assert_eq!(g.get("/api/me").await.status, 401, "Sitzungen beendet");
    let mut c = env.client();
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "gerda", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.status, 403, "Passwort allein reicht nie: {}", r.body);
    let (mut g, _, _) = setup_with_totp(&env, url.split('#').nth(1).unwrap()).await;
    assert_eq!(g.get("/api/me").await.status, 200);

    let r = a
        .post(
            &format!("/api/admin/users/{gid}/disabled"),
            json!({"disabled": true}),
        )
        .await;
    r.ok();
    assert_eq!(g.get("/api/me").await.status, 401);
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "gerda", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.status, 401);
    let own = users
        .body
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "admin")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let r = a
        .post(
            &format!("/api/admin/users/{own}/disabled"),
            json!({"disabled": true}),
        )
        .await;
    assert_eq!(r.status, 400, "nicht sich selbst sperren");
    env.finish().await;
}

#[tokio::test]
async fn sitzungen_einsehen_und_beenden() {
    let env = env_or_skip!();
    let (mut a, secret, _) = setup_with_totp(&env, &env.invite("hans", false).await).await;
    let mut b = login_totp(&env, "hans", &secret).await;
    let list = a.get("/api/me/sessions").await;
    let list = list.ok().as_array().unwrap().clone();
    assert_eq!(list.len(), 2);
    let other = list.iter().find(|s| s["current"] == false).unwrap()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        a.send("DELETE", &format!("/api/me/sessions/{other}"), None)
            .await
            .status,
        204
    );
    assert_eq!(b.get("/api/me").await.status, 401);
    assert_eq!(a.get("/api/me").await.status, 200);
    env.finish().await;
}

#[tokio::test]
async fn passwort_aendern_beendet_andere_sitzungen() {
    let env = env_or_skip!();
    let (mut a, secret, _) = setup_with_totp(&env, &env.invite("ida", false).await).await;
    let mut b = login_totp(&env, "ida", &secret).await;
    let r = a
        .post(
            "/api/me/password",
            json!({"password": "Neues langes Passwort 42"}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(b.get("/api/me").await.status, 401);
    let mut c = env.client();
    let r = c
        .post(
            "/api/auth/login",
            json!({"username": "ida", "password": PASSWORD}),
        )
        .await;
    assert_eq!(r.status, 401);
    env.finish().await;
}

fn authenticator() -> WebauthnAuthenticator<SoftPasskey> {
    // Der Software-Authenticator meldet eine Nutzerprüfung (wie Face ID/Touch ID).
    WebauthnAuthenticator::new(SoftPasskey::new(true))
}

#[tokio::test]
async fn passkey_einrichtung_anmeldung_und_step_up() {
    let env = env_or_skip!();
    let origin: Url = ORIGIN.parse().unwrap();
    let mut key = authenticator();
    let invite = env.invite("jana", false).await;
    let mut c = env.client();
    let t = c
        .post("/api/setup/start", json!({"invite": invite}))
        .await
        .ok()["setup_token"]
        .as_str()
        .unwrap()
        .to_owned();
    c.post(
        "/api/setup/password",
        json!({"setup_token": t, "password": PASSWORD}),
    )
    .await
    .ok();
    let begin = c
        .post(
            "/api/setup/passkey/begin",
            json!({"setup_token": t, "name": "iPhone"}),
        )
        .await;
    let options: CreationChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let credential = key
        .do_registration(origin.clone(), options)
        .expect("Registrierung");
    let done = c
        .post(
            "/api/setup/passkey/finish",
            json!({"setup_token": t, "ceremony": begin.body["ceremony"], "credential": credential}),
        )
        .await;
    assert_eq!(done.ok()["me"]["passkeys"][0]["name"], "iPhone");
    assert_eq!(done.body["recovery_codes"].as_array().unwrap().len(), 10);

    // Anmeldung nur mit Passkey.
    let mut p = env.client();
    let begin = p
        .post("/api/auth/passkey/begin", json!({"username": "jana"}))
        .await;
    let options: RequestChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = key
        .do_authentication(origin.clone(), options)
        .expect("Anmeldung");
    let r = p
        .post(
            "/api/auth/passkey/finish",
            json!({"ceremony": begin.body["ceremony"], "credential": cred}),
        )
        .await;
    assert_eq!(r.ok()["username"], "jana");

    // Derselbe Vorgang lässt sich nicht wiederholen.
    let r = p
        .post(
            "/api/auth/passkey/finish",
            json!({"ceremony": begin.body["ceremony"], "credential": cred}),
        )
        .await;
    assert_eq!(r.status, 401);

    // Step-up per Passkey.
    sqlx::query("UPDATE sessions SET step_up_at = NULL")
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(p.get("/api/me").await.ok()["step_up_valid"], false);
    let begin = p.post("/api/auth/step-up/passkey/begin", json!({})).await;
    let options: RequestChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = key.do_authentication(origin.clone(), options).unwrap();
    let r = p
        .post(
            "/api/auth/step-up/passkey/finish",
            json!({"ceremony": begin.body["ceremony"], "credential": cred}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(p.get("/api/me").await.ok()["step_up_valid"], true);

    // Passwort + Passkey als zweiter Faktor.
    let mut q = env.client();
    let r = q
        .post(
            "/api/auth/login",
            json!({"username": "jana", "password": PASSWORD}),
        )
        .await;
    assert_eq!(
        (r.ok()["totp"].clone(), r.body["passkey"].clone()),
        (json!(false), json!(true))
    );
    let begin = q
        .post(
            "/api/auth/passkey/begin",
            json!({"challenge": r.body["challenge"]}),
        )
        .await;
    let options: RequestChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = key.do_authentication(origin.clone(), options).unwrap();
    q.post(
        "/api/auth/passkey/finish",
        json!({"ceremony": begin.body["ceremony"], "credential": cred}),
    )
    .await
    .ok();

    // Der einzige zweite Faktor lässt sich nicht entfernen.
    let me = p.get("/api/me").await;
    let pk = me.ok()["passkeys"][0]["id"].as_i64().unwrap();
    let r = p
        .send("DELETE", &format!("/api/me/passkeys/{pk}"), None)
        .await;
    assert_eq!(r.status, 409);
    env.finish().await;
}

#[tokio::test]
async fn fremder_passkey_wird_abgelehnt() {
    let env = env_or_skip!();
    let origin: Url = ORIGIN.parse().unwrap();
    // Konto mit TOTP und Passkey A; ein anderer Passkey B darf nicht anmelden.
    let (mut c, _, _) = setup_with_totp(&env, &env.invite("kai", false).await).await;
    let mut a = authenticator();
    let begin = c.post("/api/me/passkeys/begin", json!({"name": "A"})).await;
    let options: CreationChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = a.do_registration(origin.clone(), options).unwrap();
    let r = c
        .post(
            "/api/me/passkeys/finish",
            json!({"ceremony": begin.body["ceremony"], "credential": cred}),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);

    let mut b = authenticator();
    let mut p = env.client();
    let begin = p
        .post("/api/auth/passkey/begin", json!({"username": "kai"}))
        .await;
    let options: RequestChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    // B kennt keinen der erlaubten Schlüssel.
    assert!(b.do_authentication(origin.clone(), options).is_err());
    let r = p
        .post("/api/auth/passkey/begin", json!({"username": "niemand"}))
        .await;
    assert_eq!(r.status, 400);
    env.finish().await;
}

#[tokio::test]
async fn passkey_von_anderer_subdomain_wird_abgelehnt() {
    let env = env_or_skip!();
    let origin: Url = ORIGIN.parse().unwrap();
    let (mut c, _, _) = setup_with_totp(&env, &env.invite("lena", false).await).await;
    let mut key = authenticator();
    let begin = c.post("/api/me/passkeys/begin", json!({"name": "A"})).await;
    let options: CreationChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = key.do_registration(origin, options).unwrap();
    c.post(
        "/api/me/passkeys/finish",
        json!({"ceremony": begin.body["ceremony"], "credential": cred}),
    )
    .await;

    // Eine andere (z.B. kompromittierte) Seite unter der Passkey-Domain darf nicht anmelden.
    let evil: Url = "https://fotos.drive.example.test".parse().unwrap();
    let mut p = env.client();
    let begin = p
        .post("/api/auth/passkey/begin", json!({"username": "lena"}))
        .await;
    let options: RequestChallengeResponse =
        serde_json::from_value(begin.ok()["options"].clone()).unwrap();
    let cred = key
        .do_authentication(evil, options)
        .expect("Authenticator signiert für die RP-ID");
    let r = p
        .post(
            "/api/auth/passkey/finish",
            json!({"ceremony": begin.body["ceremony"], "credential": cred}),
        )
        .await;
    assert_eq!(r.status, 401, "{}", r.body);
    env.finish().await;
}

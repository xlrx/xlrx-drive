//! Public links (PLAN 9.2): what a link opens for someone without an account, and nothing more;
//! password, lockout, expiry, download limit; uploads that never overwrite; and a link never
//! outlives the rights of the person who made it.

mod common;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::files::*;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;

/// Someone with a link: no session, only the cookie of an unlocked link.
struct Visitor {
    app: Router,
    cookie: Option<String>,
}

struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    bytes: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.bytes).unwrap_or(Value::Null)
    }
    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

impl Visitor {
    fn new(env: &Env) -> Self {
        Self {
            app: env.app.clone(),
            cookie: None,
        }
    }

    async fn req(
        &mut self,
        method: &str,
        path: &str,
        body: Option<Body>,
        json_body: bool,
        headers: &[(&str, &str)],
    ) -> Answer {
        let mut r = Request::builder()
            .method(method)
            .uri(path)
            .header(header::ORIGIN, ORIGIN);
        if json_body {
            r = r.header(header::CONTENT_TYPE, "application/json");
        }
        if let Some(c) = &self.cookie {
            r = r.header(header::COOKIE, format!("xlrx_link={c}"));
        }
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        let res = self
            .app
            .clone()
            .oneshot(r.body(body.unwrap_or_else(Body::empty)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        for v in headers.get_all(header::SET_COOKIE) {
            let v = v.to_str().unwrap();
            if let Some(rest) = v.strip_prefix("xlrx_link=") {
                self.cookie = Some(rest.split(';').next().unwrap().to_owned());
            }
        }
        let bytes = res.into_body().collect().await.unwrap().to_bytes().to_vec();
        Answer {
            status,
            headers,
            bytes,
        }
    }

    async fn get(&mut self, path: &str) -> Answer {
        self.req("GET", path, None, false, &[]).await
    }

    async fn get_with(&mut self, path: &str, headers: &[(&str, &str)]) -> Answer {
        self.req("GET", path, None, false, headers).await
    }

    async fn post(&mut self, path: &str, body: Value) -> Answer {
        self.req("POST", path, Some(Body::from(body.to_string())), true, &[])
            .await
    }

    async fn send(&mut self, method: &str, path: &str, bytes: &[u8]) -> Answer {
        self.req(method, path, Some(Body::from(bytes.to_vec())), false, &[])
            .await
    }
}

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

/// Makes a link and returns its token.
async fn link(c: &mut Client, id: i64, body: Value) -> String {
    let r = c.post(&format!("/api/nodes/{id}/links"), body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    let url = r.ok()["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with(&format!("{ORIGIN}/s/")), "{url}");
    url.rsplit('/').next().unwrap().to_owned()
}

async fn audit_count(env: &Env, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = $1")
        .bind(action)
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn ansehen_herunterladen_und_nicht_mehr() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, root_node, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Rechnungen/Strom.pdf"), b"%PDF-1.4 Strom");
    write(&dir.join("Haus/Notiz.txt"), b"Heizung warten");
    write(&dir.join("Haus/Plan.zip"), b"PK......");
    write(&dir.join("Geheim/Tagebuch.txt"), b"privat");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let strom = node(&env, &home, "Haus/Rechnungen/Strom.pdf").await.id;
    let notiz = node(&env, &home, "Haus/Notiz.txt").await.id;
    let plan = node(&env, &home, "Haus/Plan.zip").await.id;
    let geheim = node(&env, &home, "Geheim/Tagebuch.txt").await.id;

    // To look at.
    let t = link(&mut klaus, haus, json!({ "kind": "view" })).await;
    assert_eq!(t.len(), 22);
    let mut v = Visitor::new(&env);
    let i = v.get(&format!("/api/public/{t}")).await;
    assert_eq!(i.status, 200);
    let i = i.json();
    assert_eq!(
        (i["kind"].as_str(), i["locked"].as_bool()),
        (Some("view"), Some(false))
    );
    assert_eq!(i["node"]["name"], "Haus");
    assert_eq!(i["owner"], "klaus Test");
    assert_eq!(
        i["can"],
        json!({"browse": true, "download": false, "upload": false, "replace": false})
    );
    let kids = v
        .get(&format!("/api/public/{t}/nodes/{haus}/children"))
        .await;
    assert_eq!(names(&kids.json()), ["Rechnungen", "Notiz.txt", "Plan.zip"]);
    let n = v
        .get(&format!("/api/public/{t}/nodes/{strom}"))
        .await
        .json();
    let path: Vec<&str> = n["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        path,
        ["Haus", "Rechnungen", "Strom.pdf"],
        "nichts über dem geteilten Ordner"
    );
    // Shown in the browser – but not handed out as a download.
    let a = v
        .get(&format!(
            "/api/public/{t}/nodes/{notiz}/content?inline=true"
        ))
        .await;
    assert_eq!(
        (a.status, a.bytes.as_slice()),
        (StatusCode::OK, &b"Heizung warten"[..])
    );
    assert!(a.header("content-disposition").starts_with("inline"));
    assert!(a.header("content-security-policy").contains("sandbox"));
    for path in [
        format!("/api/public/{t}/nodes/{notiz}/content"),
        format!("/api/public/{t}/nodes/{plan}/content?inline=true"),
    ] {
        assert_eq!(v.get(&path).await.status, 403, "{path}");
    }
    let up = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=x.txt&size=1"),
            b"x",
        )
        .await;
    assert_eq!(up.status, 403);
    // Nothing outside "Haus", neither beside nor above it.
    for id in [geheim, root_node] {
        assert_eq!(
            v.get(&format!("/api/public/{t}/nodes/{id}")).await.status,
            404
        );
        assert_eq!(
            v.get(&format!("/api/public/{t}/nodes/{id}/content?inline=true"))
                .await
                .status,
            404
        );
    }

    // To download, at most twice.
    let t2 = link(
        &mut klaus,
        notiz,
        json!({ "kind": "download", "max_downloads": 2 }),
    )
    .await;
    let i = v.get(&format!("/api/public/{t2}")).await.json();
    assert_eq!(i["downloads_left"], 2);
    let a = v
        .get(&format!("/api/public/{t2}/nodes/{notiz}/content"))
        .await;
    assert_eq!(
        (a.status, a.bytes.as_slice()),
        (StatusCode::OK, &b"Heizung warten"[..])
    );
    assert!(a.header("content-disposition").starts_with("attachment"));
    // A resumed download is the same download.
    let a = v
        .get_with(
            &format!("/api/public/{t2}/nodes/{notiz}/content"),
            &[("range", "bytes=8-")],
        )
        .await;
    assert_eq!(
        (a.status, a.bytes.as_slice()),
        (StatusCode::PARTIAL_CONTENT, &b"warten"[..])
    );
    assert_eq!(
        v.get_with(
            &format!("/api/public/{t2}/nodes/{notiz}/content"),
            &[("range", "bytes=0-3")]
        )
        .await
        .status,
        206
    );
    let a = v
        .get(&format!("/api/public/{t2}/nodes/{notiz}/content"))
        .await;
    assert_eq!(a.status, 410, "{}", a.json());
    // Only the item itself: the folder around it stays closed.
    assert_eq!(
        v.get(&format!("/api/public/{t2}/nodes/{haus}/children"))
            .await
            .status,
        404
    );
    let list = klaus
        .get(&format!("/api/nodes/{notiz}/links"))
        .await
        .ok()
        .clone();
    assert_eq!(
        (
            list[0]["downloads"].as_i64(),
            list[0]["max_downloads"].as_i64()
        ),
        (Some(2), Some(2))
    );
    assert_eq!(audit_count(&env, "link_downloaded").await, 2);
    assert_eq!(audit_count(&env, "link_created").await, 2);

    // Ended: gone at once.
    let id = klaus.get(&format!("/api/nodes/{haus}/links")).await.ok()[0]["id"].clone();
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/links/{id}"), None)
            .await
            .status,
        204
    );
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 404);
    assert_eq!(
        v.get(&format!(
            "/api/public/{t}/nodes/{notiz}/content?inline=true"
        ))
        .await
        .status,
        404
    );
    assert_eq!(audit_count(&env, "link_removed").await, 1);

    // Expired.
    let t3 = link(
        &mut klaus,
        haus,
        json!({ "kind": "view", "expires_at": "2099-01-01T00:00:00Z" }),
    )
    .await;
    assert_eq!(v.get(&format!("/api/public/{t3}")).await.status, 200);
    sqlx::query("UPDATE links SET expires_at = now() - interval '1 minute'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let a = v.get(&format!("/api/public/{t3}")).await;
    assert_eq!(
        (a.status, a.json()["error"].as_str()),
        (StatusCode::GONE, Some("Dieser Link ist abgelaufen."))
    );
    env.finish().await;
}

#[tokio::test]
async fn nur_wer_verwaltet_mit_frischer_bestaetigung() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, root_node, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Notiz.txt"), b"x");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let notiz = node(&env, &home, "Haus/Notiz.txt").await.id;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let bert_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'bert'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    let share = klaus
        .post(
            &format!("/api/nodes/{haus}/shares"),
            json!({"type": "user", "id": bert_id, "role": "editor"}),
        )
        .await
        .ok()
        .clone();

    // Editing is not enough to hand something to strangers.
    let r = bert
        .post(&format!("/api/nodes/{haus}/links"), json!({"kind": "view"}))
        .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        bert.get(&format!("/api/nodes/{haus}/links")).await.status,
        403
    );
    // … nor to end a link someone else handed out.
    link(&mut klaus, haus, json!({"kind": "view"})).await;
    let own: i64 = sqlx::query_scalar("SELECT id FROM links")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    let r = bert
        .send("DELETE", &format!("/api/links/{own}"), None)
        .await;
    assert_eq!(r.status, 403);
    // Wrong requests.
    for (id, body) in [
        (root_node, json!({"kind": "view"})),
        (notiz, json!({"kind": "upload"})),
        (haus, json!({"kind": "alles"})),
        (haus, json!({"kind": "view", "max_downloads": 3})),
        (haus, json!({"kind": "download", "max_downloads": 0})),
        (haus, json!({"kind": "view", "password": "kurz"})),
        (
            haus,
            json!({"kind": "view", "expires_at": "2001-01-01T00:00:00Z"}),
        ),
    ] {
        let r = klaus
            .post(&format!("/api/nodes/{id}/links"), body.clone())
            .await;
        assert_eq!(r.status, 400, "{body}");
    }
    // A second factor confirmed long ago is not enough.
    sqlx::query(
        "UPDATE sessions SET step_up_at = now() - interval '1 day'
          WHERE user_id = (SELECT id FROM users WHERE username = 'klaus')",
    )
    .execute(&env.db.pool)
    .await
    .unwrap();
    let r = klaus
        .post(&format!("/api/nodes/{haus}/links"), json!({"kind": "view"}))
        .await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".to_string())
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM links")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(n, 1, "nur der vorher angelegte");

    // Managing is: bert hands out a file request.
    klaus
        .send(
            "PATCH",
            &format!("/api/shares/{}", share["id"]),
            Some(json!({"role": "manager", "keep_expiry": true})),
        )
        .await
        .ok();
    let t = link(&mut bert, haus, json!({"kind": "upload"})).await;
    let mut v = Visitor::new(&env);
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 200);
    // The link never outlives bert's rights: viewing is not enough for adding files …
    klaus
        .send(
            "PATCH",
            &format!("/api/shares/{}", share["id"]),
            Some(json!({"role": "viewer", "keep_expiry": true})),
        )
        .await
        .ok();
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 404);
    let up = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=a.txt&size=1"),
            b"a",
        )
        .await;
    assert_eq!(up.status, 404);
    // … it works again with the rights …
    klaus
        .send(
            "PATCH",
            &format!("/api/shares/{}", share["id"]),
            Some(json!({"role": "editor", "keep_expiry": true})),
        )
        .await
        .ok();
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 200);
    // … and not for a disabled account, or once the folder is in the trash.
    sqlx::query("UPDATE users SET disabled_at = now() WHERE id = $1")
        .bind(bert_id)
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 404);
    sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = $1")
        .bind(bert_id)
        .execute(&env.db.pool)
        .await
        .unwrap();
    // (A fresh step-up for klaus is not needed to delete.)
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{haus}"), None)
            .await
            .status,
        204
    );
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 404);
    env.finish().await;
}

#[tokio::test]
async fn passwort_und_sperre() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Notiz.txt"), b"Heizung");
    write(&dir.join("Fotos/a.txt"), b"a");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let fotos = node(&env, &home, "Fotos").await.id;
    let notiz = node(&env, &home, "Haus/Notiz.txt").await.id;
    let t = link(
        &mut klaus,
        haus,
        json!({"kind": "download", "password": "Gartenzaun 42"}),
    )
    .await;
    let t2 = link(
        &mut klaus,
        fotos,
        json!({"kind": "view", "password": "Gartenzaun 42"}),
    )
    .await;
    let list = klaus
        .get(&format!("/api/nodes/{haus}/links"))
        .await
        .ok()
        .clone();
    assert_eq!(list[0]["password"], true);

    // Nothing until the password is given – not even the name.
    let mut v = Visitor::new(&env);
    let i = v.get(&format!("/api/public/{t}")).await.json();
    assert_eq!(
        (i["locked"].as_bool(), &i["node"], &i["owner"]),
        (Some(true), &Value::Null, &Value::Null)
    );
    let a = v
        .get(&format!("/api/public/{t}/nodes/{haus}/children"))
        .await;
    assert_eq!(
        (a.status, a.json()["reason"].as_str()),
        (StatusCode::UNAUTHORIZED, Some("password_required"))
    );
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{notiz}/content"))
            .await
            .status,
        401
    );

    // Guessing locks the link.
    for _ in 0..5 {
        let a = v
            .post(
                &format!("/api/public/{t}/unlock"),
                json!({"password": "falsch"}),
            )
            .await;
        assert_eq!(a.status, 401);
    }
    let a = v
        .post(
            &format!("/api/public/{t}/unlock"),
            json!({"password": "Gartenzaun 42"}),
        )
        .await;
    assert_eq!(a.status, 429, "gesperrt, auch mit dem richtigen Passwort");
    assert!(v.cookie.is_none());
    assert_eq!(audit_count(&env, "link_password_failed").await, 5);
    sqlx::query("DELETE FROM auth_throttle WHERE key LIKE 'link:%'")
        .execute(&env.db.pool)
        .await
        .unwrap();

    let a = v
        .post(
            &format!("/api/public/{t}/unlock"),
            json!({"password": "Gartenzaun 42"}),
        )
        .await;
    assert_eq!(a.status, 204);
    let set = a.header("set-cookie").to_owned();
    assert!(
        set.contains(&format!("Path=/api/public/{t};"))
            && set.contains("HttpOnly")
            && set.contains("SameSite=Strict")
            && set.contains("Secure"),
        "{set}"
    );
    assert_eq!(
        v.get(&format!("/api/public/{t}")).await.json()["node"]["name"],
        "Haus"
    );
    assert_eq!(
        names(
            &v.get(&format!("/api/public/{t}/nodes/{haus}/children"))
                .await
                .json()
        ),
        ["Notiz.txt"]
    );
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{notiz}/content"))
            .await
            .bytes,
        b"Heizung"
    );
    // The unlocking counts for this link only (same password or not).
    assert_eq!(
        v.get(&format!("/api/public/{t2}/nodes/{fotos}/children"))
            .await
            .status,
        401
    );
    // A forged or altered cookie opens nothing.
    let good = v.cookie.clone().unwrap();
    let last = if good.ends_with('A') { 'B' } else { 'A' };
    v.cookie = Some(format!("{}{last}", &good[..good.len() - 1]));
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{haus}/children"))
            .await
            .status,
        401
    );
    // Changing the password (here: in the database) ends earlier unlockings.
    v.cookie = Some(good);
    sqlx::query("UPDATE links l SET password_hash = (SELECT o.password_hash FROM links o WHERE o.id <> l.id LIMIT 1)")
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{haus}/children"))
            .await
            .status,
        401
    );
    env.finish().await;
}

#[tokio::test]
async fn raten_sperrt_die_adresse() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Notiz.txt"), b"x");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let t = link(&mut klaus, haus, json!({"kind": "view"})).await;
    let mut v = Visitor::new(&env);
    // Not even looked up: other shapes than a token.
    assert_eq!(v.get("/api/public/..%2F..%2Fetc").await.status, 404);
    for i in 0..19 {
        let fake = format!("{:0>22}", format!("Rate{i}"));
        assert_eq!(v.get(&format!("/api/public/{fake}")).await.status, 404);
    }
    // After enough wrong guesses the address is locked out, also for real links.
    assert_eq!(v.get(&format!("/api/public/{t}")).await.status, 429);
    env.finish().await;
}

#[tokio::test]
async fn hochladen_ohne_ueberschreiben() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Notiz.txt"), b"alt");
    write(&dir.join("Haus/Keller/Plan.txt"), b"plan");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let keller = node(&env, &home, "Haus/Keller").await.id;
    let notiz = node(&env, &home, "Haus/Notiz.txt").await;

    // A file request: add, without seeing anything.
    let t = link(&mut klaus, haus, json!({"kind": "upload"})).await;
    let mut v = Visitor::new(&env);
    let i = v.get(&format!("/api/public/{t}")).await.json();
    assert_eq!(
        i["can"],
        json!({"browse": false, "download": false, "upload": true, "replace": false})
    );
    assert_eq!(i["node"]["name"], "Haus");
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{haus}/children"))
            .await
            .status,
        403
    );
    assert_eq!(
        v.get(&format!("/api/public/{t}/nodes/{haus}")).await.status,
        403
    );
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=Notiz.txt&size=3"),
            b"neu",
        )
        .await;
    assert_eq!(a.status, 201);
    assert_eq!(
        a.json(),
        json!({"name": "Notiz.txt"}),
        "verrät nicht, dass es den Namen schon gab"
    );
    assert_eq!(std::fs::read(dir.join("Haus/Notiz.txt")).unwrap(), b"alt");
    assert_eq!(
        std::fs::read(dir.join("Haus/Notiz (1).txt")).unwrap(),
        b"neu"
    );
    // Only into the folder itself.
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{keller}/files?name=b.txt&size=1"),
            b"b",
        )
        .await;
    assert_eq!(a.status, 404);
    // Size announced and within the limit (10 MB in tests).
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=c.txt"),
            b"c",
        )
        .await;
    assert_eq!(a.status, 400);
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=c.txt&size=20000000"),
            b"c",
        )
        .await;
    assert_eq!(a.status, 400);
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{haus}/files?name=c.txt&size=5"),
            b"c",
        )
        .await;
    assert_eq!(a.status, 400, "kürzer als angekündigt");
    assert!(!dir.join("Haus/c.txt").exists());
    assert_eq!(audit_count(&env, "link_uploaded").await, 1);

    // To edit: new files anywhere inside, new contents with the old kept as a version.
    let t = link(&mut klaus, haus, json!({"kind": "edit"})).await;
    let a = v
        .send(
            "POST",
            &format!("/api/public/{t}/nodes/{keller}/files?name=Plan.txt&size=2"),
            b"p2",
        )
        .await;
    assert_eq!(
        (a.status, a.json()["name"].as_str()),
        (StatusCode::CREATED, Some("Plan (1).txt"))
    );
    let a = v
        .send(
            "PUT",
            &format!(
                "/api/public/{t}/nodes/{}/content?base_rev={}&size=6",
                notiz.id, notiz.rev
            ),
            b"Neues!",
        )
        .await;
    assert_eq!(a.status, 200, "{}", a.json());
    assert_eq!(
        std::fs::read(dir.join("Haus/Notiz.txt")).unwrap(),
        b"Neues!"
    );
    let versions = klaus
        .get(&format!("/api/nodes/{}/versions", notiz.id))
        .await
        .ok()
        .clone();
    assert_eq!(
        versions.as_array().unwrap().len(),
        1,
        "die alte Fassung bleibt"
    );
    // Based on an old state: refused, nothing lost.
    let a = v
        .send(
            "PUT",
            &format!(
                "/api/public/{t}/nodes/{}/content?base_rev={}&size=1",
                notiz.id, notiz.rev
            ),
            b"x",
        )
        .await;
    assert_eq!(a.status, 409);
    assert_eq!(
        std::fs::read(dir.join("Haus/Notiz.txt")).unwrap(),
        b"Neues!"
    );
    // A link cannot delete, rename or move anything: there is no such way at all.
    for (m, p) in [
        ("DELETE", format!("/api/public/{t}/nodes/{}", notiz.id)),
        ("PATCH", format!("/api/public/{t}/nodes/{}", notiz.id)),
    ] {
        let a = v.send(m, &p, b"{}").await;
        assert!(a.status == 405 || a.status == 404, "{m} {p}: {}", a.status);
    }
    // Upload-only and view links cannot store new contents.
    let tv = link(&mut klaus, haus, json!({"kind": "download"})).await;
    let a = v
        .send(
            "PUT",
            &format!(
                "/api/public/{tv}/nodes/{}/content?base_rev=1&size=1",
                notiz.id
            ),
            b"x",
        )
        .await;
    assert_eq!(a.status, 403);
    env.finish().await;
}

//! The bell (PLAN 8.4): sharing tells the people concerned, live; files arriving through a file
//! request are gathered into one notice; only what the person may still see is shown.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use common::files::*;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use xlrx_server::files::db;
use xlrx_server::files::roots;

async fn user_id(env: &Env, name: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(name)
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

/// The next `notification` event of an event stream.
async fn next_bell(body: &mut Body) -> Value {
    let mut buf = String::new();
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(10), body.frame())
            .await
            .expect("Ereignis kam nicht")
            .expect("Strom zu Ende")
            .unwrap();
        if let Ok(data) = frame.into_data() {
            buf.push_str(&String::from_utf8_lossy(&data));
        }
        while let Some(end) = buf.find("\n\n") {
            let event: String = buf.drain(..end + 2).collect();
            if event.lines().any(|l| l == "event: notification") {
                let data = event
                    .lines()
                    .find_map(|l| l.strip_prefix("data: "))
                    .unwrap();
                return serde_json::from_str(data).unwrap();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teilen_meldet_sich_live() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Urlaub/a.jpg"), b"a");
    write(&dir.join("Haus/b.txt"), b"b");
    let home = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let urlaub = node(&env, &home, "Urlaub").await.id;
    let haus = node(&env, &home, "Haus").await.id;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let (mut anna, _, _, _) = signed_in(&env, "anna").await;
    let (bert_id, anna_id, klaus_id) = (
        user_id(&env, "bert").await,
        user_id(&env, "anna").await,
        user_id(&env, "klaus").await,
    );
    assert_eq!(bert.get("/api/notifications").await.ok()["unread"], 0);

    // Bert listens.
    let req = Request::builder()
        .uri("/api/sync/notify")
        .header(
            header::COOKIE,
            format!("xlrx_session={}", bert.cookie.clone().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    let mut stream = env.app.clone().oneshot(req).await.unwrap().into_body();

    let s = klaus
        .post(
            &format!("/api/nodes/{urlaub}/shares"),
            json!({"type": "user", "id": bert_id, "role": "editor"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(next_bell(&mut stream).await, json!({"unread": 1}));
    let b = bert.get("/api/notifications").await.ok().clone();
    assert_eq!(b["unread"], 1);
    let n = &b["items"][0];
    assert_eq!(
        (
            n["kind"].as_str(),
            n["actor"].as_str(),
            n["node"]["name"].as_str(),
            n["read"].as_bool()
        ),
        (
            Some("shared"),
            Some("klaus Test"),
            Some("Urlaub"),
            Some(false)
        )
    );
    assert_eq!(n["details"]["role"], "editor");

    // A group: everyone in it but the one sharing.
    let g: i64 = sqlx::query_scalar(
        "INSERT INTO groups (name, name_folded) VALUES ('Familie', 'familie') RETURNING id",
    )
    .fetch_one(&env.db.pool)
    .await
    .unwrap();
    for u in [bert_id, anna_id, klaus_id] {
        sqlx::query("INSERT INTO group_members (group_id, user_id) VALUES ($1, $2)")
            .bind(g)
            .bind(u)
            .execute(&env.db.pool)
            .await
            .unwrap();
    }
    let r = klaus
        .post(
            &format!("/api/nodes/{haus}/shares"),
            json!({"type": "group", "id": g, "role": "viewer"}),
        )
        .await;
    assert_eq!(r.status, 201);
    assert_eq!(next_bell(&mut stream).await, json!({"unread": 2}));
    let a = anna.get("/api/notifications").await.ok().clone();
    assert_eq!(
        (
            a["unread"].as_i64(),
            a["items"][0]["details"]["group"].as_str()
        ),
        (Some(1), Some("Familie"))
    );
    assert_eq!(
        klaus.get("/api/notifications").await.ok()["unread"],
        0,
        "nicht an sich selbst"
    );

    // Read: one, then all; the other windows hear it.
    let id = bert.get("/api/notifications").await.ok()["items"][1]["id"].clone();
    assert_eq!(
        bert.post("/api/notifications/read", json!({"ids": [id]}))
            .await
            .status,
        204
    );
    assert_eq!(next_bell(&mut stream).await, json!({"unread": 1}));
    assert_eq!(
        bert.post("/api/notifications/read", json!({})).await.status,
        204
    );
    assert_eq!(next_bell(&mut stream).await, json!({"unread": 0}));
    let b = bert.get("/api/notifications").await.ok().clone();
    assert!(
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["read"] == true)
    );

    // Only what may still be seen.
    klaus
        .send("DELETE", &format!("/api/shares/{}", s["id"]), None)
        .await;
    let names: Vec<String> = bert.get("/api/notifications").await.ok()["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["node"]["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["Haus"]);
    // Anna cannot mark bert's.
    let bert_n: i64 = sqlx::query_scalar("SELECT id FROM notifications WHERE user_id = $1 LIMIT 1")
        .bind(bert_id)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE notifications SET read_at = NULL")
        .execute(&env.db.pool)
        .await
        .unwrap();
    anna.post("/api/notifications/read", json!({"ids": [bert_n]}))
        .await;
    let unread: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT read_at FROM notifications WHERE id = $1")
            .bind(bert_n)
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert!(unread.is_none());
    env.finish().await;
}

#[tokio::test]
async fn dateianfrage_sammelt_eingaenge() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Belege/.keep"), b"");
    let home = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let belege = node(&env, &home, "Belege").await.id;
    let r = klaus
        .post(
            &format!("/api/nodes/{belege}/links"),
            json!({"kind": "upload"}),
        )
        .await;
    let token = r.body["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let send = |name: &'static str| {
        let app = env.app.clone();
        let token = token.clone();
        async move {
            let req = Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/public/{token}/nodes/{belege}/files?name={name}&size=1"
                ))
                .header(header::ORIGIN, ORIGIN)
                .body(Body::from("x"))
                .unwrap();
            assert_eq!(app.oneshot(req).await.unwrap().status(), 201);
        }
    };
    send("a.pdf").await;
    send("b.pdf").await;
    let b = klaus.get("/api/notifications").await.ok().clone();
    assert_eq!(b["unread"], 1, "eine Meldung für beide");
    let n = &b["items"][0];
    assert_eq!(n["kind"], "link_upload");
    assert_eq!(
        (n["details"]["count"].as_i64(), n["node"]["name"].as_str()),
        (Some(2), Some("Belege"))
    );
    assert_eq!(n["details"]["names"], json!(["a.pdf", "b.pdf"]));
    // Once read, the next one is news again.
    klaus.post("/api/notifications/read", json!({})).await;
    send("c.pdf").await;
    let b = klaus.get("/api/notifications").await.ok().clone();
    assert_eq!(
        (b["unread"].as_i64(), b["items"].as_array().unwrap().len()),
        (Some(1), 2)
    );
    assert_eq!(b["items"][0]["details"]["names"], json!(["c.pdf"]));
    env.finish().await;
}

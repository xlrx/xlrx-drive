//! Start page (PLAN 8.1, 8.2): what a person opens is noted for them alone; suggestions say why,
//! and only ever show what is still there and may still be seen.

mod common;

use std::collections::HashMap;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

async fn events(env: &Env, node: i64) -> Vec<(String, String)> {
    sqlx::query_as("SELECT kind, source FROM access_events WHERE node_id = $1 ORDER BY at")
        .bind(node)
        .fetch_all(&env.db.pool)
        .await
        .unwrap()
}

/// Opened by `user` at `ago` (an SQL interval) – history the test needs.
async fn opened_ago(env: &Env, user: &str, node: i64, ago: &str) {
    sqlx::query(
        "INSERT INTO access_events (user_id, node_id, kind, source, at)
         SELECT id, $2, 'open', 'web', now() - $3::interval FROM users WHERE username = $1",
    )
    .bind(user)
    .bind(node)
    .bind(ago)
    .execute(&env.db.pool)
    .await
    .unwrap();
}

/// Name → reason of each suggestion.
fn reasons(v: &Value) -> HashMap<String, Value> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| (s["name"].as_str().unwrap().to_owned(), s["reason"].clone()))
        .collect()
}

#[tokio::test]
async fn oeffnen_und_herunterladen_werden_vermerkt() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/a.txt"), b"Heizung warten");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let a = node(&env, &home, "Haus/a.txt").await.id;
    let open = |c: &'static str| (format!("/api/nodes/{a}/opened"), c);

    assert_eq!(klaus.post(&open("").0, json!({})).await.status, 204);
    assert_eq!(klaus.post(&open("").0, json!({})).await.status, 204);
    assert_eq!(
        events(&env, a).await,
        [("open".into(), "web".into())],
        "einmal je 10 Minuten"
    );
    // The Mac reports what was opened locally – also later, but not from the future or long ago.
    let two_days = (time::OffsetDateTime::now_utc() - time::Duration::days(2))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let r = klaus
        .post(&open("").0, json!({"source": "mac", "at": two_days}))
        .await;
    assert_eq!(r.status, 204);
    for body in [
        json!({"source": "mac", "at": "2099-01-01T00:00:00Z"}),
        json!({"source": "mac", "at": "2001-01-01T00:00:00Z"}),
        json!({"source": "fax"}),
    ] {
        assert_eq!(
            klaus.post(&open("").0, body.clone()).await.status,
            400,
            "{body}"
        );
    }
    // Downloads count; previews and resumed downloads do not.
    assert_eq!(
        klaus
            .get_raw(&format!("/api/nodes/{a}/content?inline=true"), &[])
            .await
            .status,
        200
    );
    assert_eq!(
        klaus
            .get_raw(&format!("/api/nodes/{a}/content"), &[("range", "bytes=5-")])
            .await
            .status,
        206
    );
    assert_eq!(events(&env, a).await.len(), 2);
    assert_eq!(
        klaus
            .get_raw(&format!("/api/nodes/{a}/content"), &[])
            .await
            .status,
        200
    );
    let kinds: Vec<String> = events(&env, a).await.into_iter().map(|e| e.0).collect();
    assert_eq!(kinds, ["open", "open", "download"]);
    // Nobody else's.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(
        bert.post(&format!("/api/nodes/{a}/opened"), json!({}))
            .await
            .status,
        404
    );
    env.finish().await;
}

#[tokio::test]
async fn vorschlaege_mit_begruendung() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    for f in ["A", "B", "C", "E", "F", "G"] {
        write(&dir.join(format!("Haus/{f}.txt")), f.as_bytes());
    }
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    // The files are older than the history below.
    sqlx::query("UPDATE journal SET at = at - interval '60 days'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let id = |n: &str| {
        let env = &env;
        let home = &home;
        let n = n.to_owned();
        async move { node(env, home, &format!("Haus/{n}.txt")).await.id }
    };
    let (a, b, c, e, f, g) = (
        id("A").await,
        id("B").await,
        id("C").await,
        id("E").await,
        id("F").await,
        id("G").await,
    );
    let haus = node(&env, &home, "Haus").await.id;

    // B: opened on this weekday at this hour in the last three weeks.
    for w in ["7 days", "14 days", "21 days"] {
        opened_ago(&env, "klaus", b, w).await;
    }
    // F and G were opened together twice; F is opened now (last).
    opened_ago(&env, "klaus", f, "10 days").await;
    opened_ago(&env, "klaus", g, "10 days - 5 minutes").await;
    opened_ago(&env, "klaus", f, "20 days").await;
    opened_ago(&env, "klaus", g, "20 days - 5 minutes").await;
    // C: known to klaus, then changed by bert.
    opened_ago(&env, "klaus", c, "3 days").await;
    // E: opened, then deleted.
    opened_ago(&env, "klaus", e, "1 hour").await;
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{e}"), None)
            .await
            .status,
        204
    );
    klaus
        .post(&format!("/api/nodes/{a}/opened"), json!({}))
        .await;
    klaus
        .post(&format!("/api/nodes/{f}/opened"), json!({}))
        .await;

    let (mut bert, bert_root, bert_node, bert_dir) = signed_in(&env, "bert").await;
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
    let cur = klaus.get(&format!("/api/nodes/{c}")).await.ok().clone();
    let r = bert
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{c}/content?base_rev={}", cur["rev"]),
            b"C2".to_vec(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // D: bert shares a file of his own with klaus.
    write(&bert_dir.join("D.txt"), b"D");
    roots::scan(&env.state, &root(&env, bert_root).await)
        .await
        .unwrap();
    let d = bert
        .get(&format!("/api/nodes/{bert_node}/children"))
        .await
        .ok()[0]["id"]
        .as_i64()
        .unwrap();
    let klaus_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'klaus'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    let r = bert
        .post(
            &format!("/api/nodes/{d}/shares"),
            json!({"type": "user", "id": klaus_id, "role": "viewer"}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);

    let s = klaus.get("/api/suggestions").await.ok().clone();
    let r = reasons(&s);
    assert_eq!(r["A.txt"]["kind"], "opened");
    assert_eq!(r["B.txt"]["kind"], "weekly");
    assert_eq!(r["B.txt"]["weeks"], 3);
    let today: i32 =
        sqlx::query_scalar("SELECT extract(isodow FROM now() AT TIME ZONE 'Europe/Berlin')::int")
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert_eq!(r["B.txt"]["weekday"], today);
    assert_eq!(r["G.txt"], json!({"kind": "together", "with": "F.txt"}));
    assert_eq!(
        (r["C.txt"]["kind"].as_str(), r["C.txt"]["who"].as_str()),
        (Some("changed"), Some("bert Test"))
    );
    assert_eq!(
        (r["D.txt"]["kind"].as_str(), r["D.txt"]["by"].as_str()),
        (Some("shared"), Some("bert Test"))
    );
    assert!(!r.contains_key("E.txt"), "im Papierkorb");
    assert_eq!(s[0]["folder"].as_str().map(|f| !f.is_empty()), Some(true));

    // What was shown is noted once an hour; opening one is noted too.
    klaus.get("/api/suggestions").await.ok();
    let shown: i64 = sqlx::query_scalar("SELECT count(*) FROM suggestion_log WHERE node_id = $1")
        .bind(a)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(shown, 1);
    assert_eq!(
        klaus
            .post("/api/suggestions/opened", json!({"node_id": a}))
            .await
            .status,
        204
    );
    let opened: Option<time::OffsetDateTime> =
        sqlx::query_scalar("SELECT opened_at FROM suggestion_log WHERE node_id = $1")
            .bind(a)
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert!(opened.is_some());

    // Bert: what he opened of klaus's is gone with the share.
    bert.post(&format!("/api/nodes/{c}/opened"), json!({}))
        .await;
    assert!(reasons(&bert.get("/api/suggestions").await.ok().clone()).contains_key("C.txt"));
    klaus
        .send("DELETE", &format!("/api/shares/{}", share["id"]), None)
        .await;
    assert!(!reasons(&bert.get("/api/suggestions").await.ok().clone()).contains_key("C.txt"));
    // Private: klaus's openings never show for bert.
    assert!(!reasons(&bert.get("/api/suggestions").await.ok().clone()).contains_key("A.txt"));
    env.finish().await;
}

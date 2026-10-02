//! Sync API: the change feed of a root (in the format of the sync engine) and live notifications.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use common::files::*;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use xlrx_proto::{Kind, NodeId, Seq};
use xlrx_sync::{Config, Engine, RemoteChange, RemoteEntry};

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

fn id(v: &Value) -> i64 {
    v["id"].as_i64().unwrap()
}

/// All pages since `cursor`, as the engine wants them: applied together.
async fn fetch(
    c: &mut Client,
    root: i64,
    mut cursor: i64,
    limit: i64,
) -> (Vec<RemoteChange>, i64, usize) {
    let mut all = Vec::new();
    let mut pages = 0;
    loop {
        let r = c
            .get(&format!(
                "/api/sync/changes?root={root}&cursor={cursor}&limit={limit}"
            ))
            .await;
        let body = r.ok().clone();
        pages += 1;
        for ch in body["changes"].as_array().unwrap() {
            all.push(RemoteChange {
                node: NodeId(ch["node"].as_u64().unwrap()),
                state: serde_json::from_value::<Option<RemoteEntry>>(ch["state"].clone()).unwrap(),
            });
        }
        cursor = body["cursor"].as_i64().unwrap();
        if !body["more"].as_bool().unwrap() {
            return (all, cursor, pages);
        }
    }
}

fn engine(root_node: i64) -> Engine {
    Engine::new(Config {
        remote_root: NodeId(root_node as u64),
        device: "Test".into(),
        local_case_insensitive: true,
        max_unconfirmed_deletes: 100,
    })
}

#[tokio::test]
async fn aenderungen_im_format_der_sync_engine() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, _dir) = signed_in(&env, "anna").await;
    let projekte = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "Projekte"}),
        )
        .await
        .ok()
        .clone();
    let plan = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{}/files?name=plan.txt", id(&projekte)),
            b"Plan".to_vec(),
        )
        .await
        .ok()
        .clone();

    // First fetch: everything live, readable by the engine.
    let (changes, cursor, _) = fetch(&mut c, root_id, 0, 1000).await;
    assert_eq!(changes.len(), 2);
    let mut e = engine(root_node);
    e.on_remote_changes(changes, Seq(cursor as u64));
    let tree = &e.state().remote;
    let p = tree.get(NodeId(id(&projekte) as u64)).unwrap();
    assert_eq!(
        (p.parent, p.name.as_str(), p.kind),
        (NodeId(root_node as u64), "Projekte", Kind::Dir)
    );
    let f = tree.get(NodeId(id(&plan) as u64)).unwrap();
    assert_eq!(f.content.unwrap().size, 4);
    assert_eq!(f.rev.0 as i64, plan["rev"].as_i64().unwrap());

    // Nothing new: empty, same cursor.
    let (none, same, _) = fetch(&mut c, root_id, cursor, 1000).await;
    assert!(none.is_empty());
    assert_eq!(same, cursor);

    // Rename, then delete the folder: only what changed, deletions as `None`.
    c.send(
        "PATCH",
        &format!("/api/nodes/{}", id(&plan)),
        Some(json!({"name": "Plan 2026.txt"})),
    )
    .await
    .ok();
    let (changes, cursor2, _) = fetch(&mut c, root_id, cursor, 1000).await;
    assert_eq!(changes.len(), 1);
    assert_eq!(
        changes[0].state.as_ref().unwrap().name.as_str(),
        "Plan 2026.txt"
    );
    e.on_remote_changes(changes, Seq(cursor2 as u64));
    assert_eq!(
        c.send("DELETE", &format!("/api/nodes/{}", id(&projekte)), None)
            .await
            .status,
        204
    );
    let (changes, cursor3, _) = fetch(&mut c, root_id, cursor2, 1000).await;
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|ch| ch.state.is_none()));
    e.on_remote_changes(changes, Seq(cursor3 as u64));
    assert_eq!(e.state().remote.len(), 0);

    // Others: not found.
    let (mut other, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(
        other
            .get(&format!("/api/sync/changes?root={root_id}"))
            .await
            .status,
        404
    );
    env.finish().await;
}

#[tokio::test]
async fn seiten_werden_zusammen_angewendet() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, _dir) = signed_in(&env, "carla").await;
    let (_, start, _) = fetch(&mut c, root_id, 0, 1000).await;
    // A folder created first, a file inside it, then the folder renamed: the folder's current
    // state comes after the file in journal order.
    let f = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "Neu"}),
        )
        .await
        .ok()
        .clone();
    let doc = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{}/files?name=doc.txt", id(&f)),
            b"d".to_vec(),
        )
        .await
        .ok()
        .clone();
    c.send(
        "PATCH",
        &format!("/api/nodes/{}", id(&f)),
        Some(json!({"name": "Umbenannt"})),
    )
    .await
    .ok();

    // One change per page: the file's page lacks its folder, so pages must be applied together.
    let (changes, cursor, pages) = fetch(&mut c, root_id, start, 1).await;
    assert_eq!(pages, 2, "one change per page");
    assert_eq!(changes[0].node, NodeId(id(&doc) as u64));
    let mut e = engine(root_node);
    e.on_remote_changes(changes, Seq(cursor as u64));
    assert!(e.state().remote.contains(NodeId(id(&doc) as u64)));
    assert_eq!(
        e.state()
            .remote
            .get(NodeId(id(&f) as u64))
            .unwrap()
            .name
            .as_str(),
        "Umbenannt"
    );
    env.finish().await;
}

/// Reads server-sent events until one of type `change` arrives.
async fn next_change(body: &mut Body) -> Value {
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
            if event.lines().any(|l| l == "event: change") {
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
async fn live_benachrichtigung_bei_aenderungen() {
    let env = env_or_skip!();
    // Subscribed right after signing in, before the root exists (it is created on first use).
    let invite = env.invite("dora", false).await;
    let (pre, _, _) = setup_with_totp(&env, &invite).await;
    let (mut c, root_id, root_node, _dir) = signed_in(&env, "dora2").await;
    let (other, other_root, _, _) = signed_in(&env, "emil").await;
    let subscribe = |cookie: String| {
        let app = env.app.clone();
        async move {
            let req = Request::builder()
                .uri("/api/sync/notify")
                .header(header::COOKIE, format!("xlrx_session={cookie}"))
                .body(Body::empty())
                .unwrap();
            let res = app.oneshot(req).await.unwrap();
            assert_eq!(res.headers()[header::CONTENT_TYPE], "text/event-stream");
            res.into_body()
        }
    };
    let mut early = subscribe(pre.cookie.clone().unwrap()).await;
    let mut mine = subscribe(c.cookie.clone().unwrap()).await;
    let mut theirs = subscribe(other.cookie.clone().unwrap()).await;

    c.post(
        &format!("/api/nodes/{root_node}/folders"),
        json!({"name": "Live"}),
    )
    .await
    .ok();
    let ev = next_change(&mut mine).await;
    assert_eq!(ev[0]["root"], root_id);
    assert!(ev[0]["seq"].as_i64().unwrap() > 0);

    // The other person hears nothing about this root, only about their own.
    let r = tokio::time::timeout(Duration::from_millis(1500), next_change(&mut theirs)).await;
    assert!(r.is_err(), "fremde Änderung gemeldet: {r:?}");
    let mut other = other;
    let other_node = other.get("/api/roots").await.ok()[0]["node_id"]
        .as_i64()
        .unwrap();
    other
        .post(
            &format!("/api/nodes/{other_node}/folders"),
            json!({"name": "Eigenes"}),
        )
        .await
        .ok();
    let ev = next_change(&mut theirs).await;
    assert_eq!(ev[0]["root"], other_root);

    // The early subscriber hears about its root once it exists.
    let mut pre = pre;
    let early_root = pre.get("/api/roots").await.ok()[0].clone();
    pre.post(
        &format!("/api/nodes/{}/folders", early_root["node_id"]),
        json!({"name": "Spät"}),
    )
    .await
    .ok();
    loop {
        let ev = next_change(&mut early).await;
        if ev
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["root"] == early_root["id"])
        {
            break;
        }
    }
    env.finish().await;
}

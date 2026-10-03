//! Activity (PLAN 8.3): who did what where, grouped; only what the person may see; a folder's
//! deletion is one entry; the first import is not news; public links show as such.

mod common;

use axum::body::Body;
use axum::http::{Request, header};
use common::files::*;
use common::*;
use serde_json::{Value, json};
use tower::ServiceExt;
use xlrx_server::files::db;
use xlrx_server::files::roots;

/// (kind, who, count, item names) per group.
fn summary(page: &Value) -> Vec<(String, String, u64, Vec<String>)> {
    page["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let who = match (&g["actor"], g["via"].as_str()) {
                (a, _) if !a.is_null() => a["name"].as_str().unwrap().to_owned(),
                (_, Some(v)) => v.to_owned(),
                _ => "?".into(),
            };
            (
                g["kind"].as_str().unwrap().to_owned(),
                who,
                g["count"].as_u64().unwrap(),
                g["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i["name"].as_str().unwrap().to_owned())
                    .collect(),
            )
        })
        .collect()
}

fn s(kind: &str, who: &str, count: u64, items: &[&str]) -> (String, String, u64, Vec<String>) {
    (
        kind.into(),
        who.into(),
        count,
        items.iter().map(|i| i.to_string()).collect(),
    )
}

async fn upload(c: &mut Client, parent: i64, name: &str, body: &[u8]) -> Value {
    let r = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{parent}/files?name={name}"),
            body.to_vec(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body
}

async fn mkdir(c: &mut Client, parent: i64, name: &str) -> i64 {
    let r = c
        .post(
            &format!("/api/nodes/{parent}/folders"),
            json!({ "name": name }),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["id"].as_i64().unwrap()
}

#[tokio::test]
async fn wer_was_wo_und_nur_was_man_sehen_darf() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    // Already there before the first import: not news.
    write(
        &env.data_dir().join("homes/klaus/Drive/Alt/alt.txt"),
        b"alt",
    );
    let (mut klaus, root_id, root_node, dir) = signed_in(&env, "klaus").await;
    let home = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(klaus.get("/api/activity").await.ok()["groups"], json!([]));

    let haus = mkdir(&mut klaus, root_node, "Haus").await;
    let a = upload(&mut klaus, haus, "a.txt", b"a").await["id"]
        .as_i64()
        .unwrap();
    let b = upload(&mut klaus, haus, "b.txt", b"b").await["id"]
        .as_i64()
        .unwrap();
    let c = upload(&mut klaus, haus, "c.txt", b"c").await;
    let keller = mkdir(&mut klaus, haus, "Keller").await;
    upload(&mut klaus, keller, "x.txt", b"x").await;
    upload(&mut klaus, keller, "y.txt", b"y").await;
    let r = klaus
        .send(
            "PATCH",
            &format!("/api/nodes/{a}"),
            Some(json!({"name": "a2.txt"})),
        )
        .await;
    assert_eq!(r.status, 200);
    let r = klaus
        .send(
            "PATCH",
            &format!("/api/nodes/{b}"),
            Some(json!({"parent_id": keller})),
        )
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{keller}"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        klaus
            .post(&format!("/api/trash/{keller}/restore"), json!({}))
            .await
            .status,
        200
    );
    // Found on the NAS (SMB, Synology Drive …): no person.
    write(&dir.join("Haus/nas.txt"), b"nas");
    roots::scan(&env.state, &home).await.unwrap();
    let r = klaus
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", c["id"], c["rev"]),
            b"c2".to_vec(),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    let page = klaus.get("/api/activity").await.ok().clone();
    assert_eq!(
        summary(&page),
        [
            s("edited", "klaus Test", 1, &["c.txt"]),
            s("uploaded", "nas", 1, &["nas.txt"]),
            s("restored", "klaus Test", 1, &["Keller"]),
            s("deleted", "klaus Test", 1, &["Keller"]),
            s("moved", "klaus Test", 1, &["b.txt"]),
            s("renamed", "klaus Test", 1, &["a2.txt"]),
            s("uploaded", "klaus Test", 3, &["y.txt", "x.txt", "b.txt"]),
            s("created", "klaus Test", 1, &["Keller"]),
            s("uploaded", "klaus Test", 2, &["c.txt", "a2.txt"]),
            s("created", "klaus Test", 1, &["Haus"]),
        ]
    );
    let g = &page["groups"][5];
    assert_eq!(g["items"][0]["prev_name"], "a.txt");
    assert_eq!(
        (g["folder"].as_str(), g["folder_id"].as_i64()),
        (Some("Meine Ablage/Haus"), Some(haus))
    );
    assert_eq!(page["groups"][0]["mine"], true);
    assert_eq!(page["groups"][1]["mine"], false);
    // Not by me; one item's history.
    let others = klaus.get("/api/activity?who=others").await.ok().clone();
    assert_eq!(summary(&others), [s("uploaded", "nas", 1, &["nas.txt"])]);
    let hist = klaus
        .get(&format!("/api/activity?node={}", c["id"]))
        .await
        .ok()
        .clone();
    assert_eq!(
        summary(&hist),
        [
            s("edited", "klaus Test", 1, &["c.txt"]),
            s("uploaded", "klaus Test", 1, &["c.txt"])
        ]
    );

    // Bert gets "Keller" to look at: he sees what happened there, and nothing above it.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let bert_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'bert'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    let r = klaus
        .post(
            &format!("/api/nodes/{keller}/shares"),
            json!({"type": "user", "id": bert_id, "role": "viewer"}),
        )
        .await;
    assert_eq!(r.status, 201);
    let r = klaus
        .post(
            &format!("/api/nodes/{keller}/links"),
            json!({"kind": "upload"}),
        )
        .await;
    assert_eq!(r.status, 201);
    let token = r.body["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    let page = bert.get("/api/activity").await.ok().clone();
    assert_eq!(
        summary(&page),
        [
            s("shared", "klaus Test", 1, &["Keller"]),
            s("restored", "klaus Test", 1, &["Keller"]),
            s("deleted", "klaus Test", 1, &["Keller"]),
            s("moved", "klaus Test", 1, &["b.txt"]),
            s("uploaded", "klaus Test", 3, &["y.txt", "x.txt", "b.txt"]),
            s("created", "klaus Test", 1, &["Keller"]),
        ],
        "keine Links (nur wer verwaltet), nichts aus „Haus“"
    );
    assert_eq!(
        page["groups"][0]["details"],
        json!({"to": "bert Test", "role": "viewer"})
    );
    for g in page["groups"].as_array().unwrap() {
        let folder = g["folder"].as_str().unwrap_or_default();
        assert!(
            folder.starts_with("Geteilt") && !folder.contains("Haus"),
            "{folder}"
        );
    }
    assert!(
        page["groups"][0]["folder_id"].is_null(),
        "Haus bleibt verborgen"
    );
    assert_eq!(page["groups"][3]["folder_id"].as_i64(), Some(keller));
    assert_eq!(
        bert.get(&format!("/api/activity?node={haus}")).await.status,
        404
    );
    assert!(
        summary(&bert.get("/api/activity?who=shares").await.ok().clone())
            .iter()
            .all(|g| g.0 == "shared")
    );

    // Through the file request: shown as that, not as klaus.
    let req = Request::builder()
        .method("POST")
        .uri(format!(
            "/api/public/{token}/nodes/{keller}/files?name=z.txt&size=1"
        ))
        .header(header::ORIGIN, ORIGIN)
        .body(Body::from("z"))
        .unwrap();
    assert_eq!(env.app.clone().oneshot(req).await.unwrap().status(), 201);
    let shares = klaus.get("/api/activity?who=shares").await.ok().clone();
    assert_eq!(
        summary(&shares),
        [
            s("link_upload", "link", 1, &["z.txt"]),
            s("link_created", "klaus Test", 1, &["Keller"]),
            s("shared", "klaus Test", 1, &["Keller"]),
        ]
    );
    let all = klaus.get("/api/activity").await.ok().clone();
    assert!(
        !summary(&all)
            .iter()
            .any(|g| g.0 == "uploaded" && g.3.contains(&"z.txt".to_string())),
        "nicht als Hochladen von klaus"
    );
    env.finish().await;
}

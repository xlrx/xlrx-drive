//! "Markiert" (PLAN 8.2): stars are private, and only what is still there and may be seen is
//! listed.

mod common;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::files::db;
use xlrx_server::files::roots;

fn names(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn markiert_privat_und_nur_was_man_sieht() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Plan.pdf"), b"p");
    write(&dir.join("Haus/Alt.txt"), b"a");
    let home = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let plan = node(&env, &home, "Haus/Plan.pdf").await.id;
    let alt = node(&env, &home, "Haus/Alt.txt").await.id;

    for id in [haus, plan, alt] {
        assert_eq!(
            klaus
                .send("PUT", &format!("/api/nodes/{id}/star"), None)
                .await
                .status,
            204
        );
    }
    // Twice is fine.
    assert_eq!(
        klaus
            .send("PUT", &format!("/api/nodes/{plan}/star"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        klaus.get(&format!("/api/nodes/{plan}")).await.ok()["starred"],
        true
    );
    let kids = klaus
        .get(&format!("/api/nodes/{haus}/children"))
        .await
        .ok()
        .clone();
    assert!(
        kids.as_array()
            .unwrap()
            .iter()
            .all(|k| k["starred"] == true)
    );
    // In the trash: not listed.
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{alt}"), None)
            .await
            .status,
        204
    );
    let list = klaus.get("/api/starred").await.ok().clone();
    assert_eq!(names(&list), ["Plan.pdf", "Haus"]);
    assert_eq!(list[0]["folder"], "Meine Ablage/Haus");
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{haus}/star"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        names(&klaus.get("/api/starred").await.ok().clone()),
        ["Plan.pdf"]
    );
    assert_eq!(
        klaus.get(&format!("/api/nodes/{haus}")).await.ok()["starred"],
        false
    );

    // Bert: only with access, only his own stars, gone with the share.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let bert_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'bert'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(
        bert.send("PUT", &format!("/api/nodes/{plan}/star"), None)
            .await
            .status,
        404
    );
    assert_eq!(
        names(&bert.get("/api/starred").await.ok().clone()),
        Vec::<String>::new()
    );
    let s = klaus
        .post(
            &format!("/api/nodes/{haus}/shares"),
            json!({"type": "user", "id": bert_id, "role": "viewer"}),
        )
        .await
        .ok()
        .clone();
    assert_eq!(
        bert.send("PUT", &format!("/api/nodes/{plan}/star"), None)
            .await
            .status,
        204
    );
    let list = bert.get("/api/starred").await.ok().clone();
    assert_eq!(
        (names(&list), list[0]["folder"].as_str()),
        (vec!["Plan.pdf".to_string()], Some("Geteilt/Haus"))
    );
    klaus
        .send("DELETE", &format!("/api/shares/{}", s["id"]), None)
        .await;
    assert_eq!(
        names(&bert.get("/api/starred").await.ok().clone()),
        Vec::<String>::new()
    );
    assert_eq!(
        names(&klaus.get("/api/starred").await.ok().clone()),
        ["Plan.pdf"]
    );
    env.finish().await;
}

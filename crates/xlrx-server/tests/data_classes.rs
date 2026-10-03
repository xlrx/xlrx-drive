//! Data classes (PLAN 7.4): "Nur lokal" by default, set per folder and inherited, changed only by
//! who manages a folder and with a fresh second factor, and asked by every cloud path.

mod common;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::files::data_class::{self, Class};
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

async fn class_of(c: &mut Client, id: i64) -> Value {
    c.get(&format!("/api/nodes/{id}")).await.ok()["data_class"].clone()
}

async fn set(c: &mut Client, id: i64, class: &str) -> u16 {
    c.send(
        "PUT",
        &format!("/api/nodes/{id}/data-class"),
        Some(json!({ "class": class })),
    )
    .await
    .status
    .as_u16()
}

async fn cloud(env: &Env, id: i64) -> bool {
    data_class::allows_cloud(&env.db.pool, env.state.cfg.default_data_class, id)
        .await
        .unwrap()
}

#[tokio::test]
async fn nur_lokal_bis_ausdruecklich_erlaubt() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut klaus, root_id, root_node, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Rechnungen/Strom.pdf"), b"%PDF");
    write(&dir.join("Haus/Vertraege/Miete.pdf"), b"%PDF");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let rechnungen = node(&env, &home, "Haus/Rechnungen").await.id;
    let vertraege = node(&env, &home, "Haus/Vertraege").await.id;
    let strom = node(&env, &home, "Haus/Rechnungen/Strom.pdf").await.id;
    let miete = node(&env, &home, "Haus/Vertraege/Miete.pdf").await.id;

    // Nothing set: nothing may leave the house.
    assert_eq!(
        class_of(&mut klaus, strom).await,
        json!({"class": "local", "explicit": false, "from": null, "default": true, "can_change": false})
    );
    assert!(!cloud(&env, strom).await);
    assert_eq!(class_of(&mut klaus, haus).await["can_change"], true);

    // Allowed for "Haus": everything inside follows …
    assert_eq!(set(&mut klaus, haus, "cloud").await, 200);
    assert!(cloud(&env, strom).await && cloud(&env, miete).await);
    let c = class_of(&mut klaus, strom).await;
    assert_eq!(
        (c["class"].as_str(), c["from"]["name"].as_str()),
        (Some("cloud"), Some("Haus"))
    );
    assert_eq!(class_of(&mut klaus, haus).await["explicit"], true);
    // … unless a folder further down says otherwise.
    assert_eq!(set(&mut klaus, vertraege, "local").await, 200);
    assert!(cloud(&env, strom).await);
    assert!(!cloud(&env, miete).await);
    let kids = klaus
        .get(&format!("/api/nodes/{haus}/children"))
        .await
        .ok()
        .clone();
    assert_eq!(find(&kids, "Vertraege")["data_class"], "local");
    assert!(find(&kids, "Rechnungen").get("data_class").is_none());
    // Back to what the folder above says.
    assert_eq!(set(&mut klaus, vertraege, "inherit").await, 200);
    assert!(cloud(&env, miete).await);
    // Also for the whole root (the default of a root), but not on files.
    assert_eq!(set(&mut klaus, root_node, "local").await, 200);
    assert_eq!(set(&mut klaus, strom, "local").await, 400);
    assert_eq!(set(&mut klaus, haus, "irgendwo").await, 400);
    assert!(cloud(&env, rechnungen).await, "Haus selbst bleibt erlaubt");

    // Every change is in the audit log.
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_log WHERE action = 'data_class_changed'")
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert_eq!(n, 4);

    // A second factor confirmed long ago is not enough.
    sqlx::query("UPDATE sessions SET step_up_at = now() - interval '1 day'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = klaus
        .send(
            "PUT",
            &format!("/api/nodes/{haus}/data-class"),
            Some(json!({"class": "local"})),
        )
        .await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".to_string())
    );
    assert!(cloud(&env, strom).await);
    env.finish().await;
}

#[tokio::test]
async fn nur_wer_verwaltet_und_nichts_verraten() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let invite = env.invite("admin", true).await;
    let (mut admin, _, _) = setup_with_totp(&env, &invite).await;
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Haus/Rechnungen/Strom.pdf"), b"%PDF");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let haus = node(&env, &home, "Haus").await.id;
    let rechnungen = node(&env, &home, "Haus/Rechnungen").await.id;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let bert_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'bert'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    set(&mut klaus, haus, "cloud").await;

    // To edit: may not decide where the data goes.
    let s = klaus
        .post(
            &format!("/api/nodes/{rechnungen}/shares"),
            json!({"type": "user", "id": bert_id, "role": "editor"}),
        )
        .await
        .ok()
        .clone();
    let c = class_of(&mut bert, rechnungen).await;
    // Allowed, but the folder above that bert cannot see is not named.
    assert_eq!(
        c,
        json!({"class": "cloud", "explicit": false, "from": null, "default": false, "can_change": false})
    );
    assert_eq!(set(&mut bert, rechnungen, "local").await, 403);
    // To manage: may.
    klaus
        .send(
            "PATCH",
            &format!("/api/shares/{}", s["id"]),
            Some(json!({"role": "manager", "keep_expiry": true})),
        )
        .await
        .ok();
    assert_eq!(class_of(&mut bert, rechnungen).await["can_change"], true);
    assert_eq!(set(&mut bert, rechnungen, "local").await, 200);
    assert_eq!(set(&mut bert, haus, "local").await, 404);

    // The administration lists every setting (record of processing).
    let o = admin.get("/api/admin/data-classes").await.ok().clone();
    assert_eq!(o["default"], "local");
    let entries: Vec<(String, String, String)> = o["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["root"].as_str().unwrap().into(),
                e["path"].as_str().unwrap().into(),
                e["class"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(
        entries,
        [
            ("Meine Ablage".into(), "Haus".into(), "cloud".into()),
            (
                "Meine Ablage".into(),
                "Haus/Rechnungen".into(),
                "local".into()
            )
        ]
    );
    assert_eq!(bert.get("/api/admin/data-classes").await.status, 403);
    env.finish().await;
}

#[tokio::test]
async fn voreinstellung_ist_einstellbar() {
    let Some(mut env) = Env::with_data().await else {
        return;
    };
    let mut cfg = env.state.cfg.clone();
    cfg.default_data_class = Class::Cloud;
    env.state = xlrx_server::AppState::new(env.db.pool.clone(), cfg).unwrap();
    env.app = xlrx_server::router(env.state.clone());
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Fotos/a.jpg"), b"x");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let a = node(&env, &home, "Fotos/a.jpg").await.id;
    assert!(cloud(&env, a).await);
    assert_eq!(class_of(&mut klaus, a).await["default"], true);
    env.finish().await;
}

//! Sharing: what people with a share may see and do in each role, that nothing above a shared
//! folder is revealed, groups, shared roots ("Geteilte Ablagen") and the checks when mounting
//! them.

mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, header};
use common::files::*;
use common::*;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

async fn uid(env: &Env, username: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(username)
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

async fn share(c: &mut Client, node: i64, to: Value, role: &str) -> Value {
    let mut body = to;
    body["role"] = json!(role);
    c.post(&format!("/api/nodes/{node}/shares"), body)
        .await
        .ok()
        .clone()
}

fn user(id: i64) -> Value {
    json!({"type": "user", "id": id})
}

fn names(v: &Value) -> Vec<String> {
    let mut n: Vec<String> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap().to_string())
        .collect();
    n.sort();
    n
}

fn crumbs(v: &Value) -> Vec<String> {
    v["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_string())
        .collect()
}

async fn status(c: &mut Client, method: &str, path: &str, body: Option<Value>) -> u16 {
    c.send(method, path, body).await.status.as_u16()
}

async fn upload(c: &mut Client, folder: i64, name: &str, data: &[u8]) -> u16 {
    c.send_bytes(
        "POST",
        &format!(
            "/api/nodes/{folder}/files?name={}&size={}",
            url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>(),
            data.len()
        ),
        data.to_vec(),
    )
    .await
    .status
    .as_u16()
}

#[tokio::test]
async fn freigabe_an_eine_person_je_rolle() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut anna, anna_root, anna_node, dir) = signed_in(&env, "anna").await;
    write(&dir.join("Projekte/Plan.txt"), b"Plan");
    write(&dir.join("Projekte/Unter/Tief.txt"), b"Tief");
    write(&dir.join("Privat/Geheim.txt"), b"Geheim");
    let home = root(&env, anna_root).await;
    roots::scan(&env.state, &home).await.unwrap();
    let projekte = node(&env, &home, "Projekte").await.id;
    let unter = node(&env, &home, "Projekte/Unter").await.id;
    let plan = node(&env, &home, "Projekte/Plan.txt").await;
    let tief = node(&env, &home, "Projekte/Unter/Tief.txt").await.id;
    let privat = node(&env, &home, "Privat").await.id;
    let geheim = node(&env, &home, "Privat/Geheim.txt").await.id;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let (mut carla, _, _, _) = signed_in(&env, "carla").await;
    let bert_id = uid(&env, "bert").await;
    let carla_id = uid(&env, "carla").await;

    // Nothing shared yet: bert sees none of it, not even that it exists.
    assert_eq!(
        status(&mut bert, "GET", &format!("/api/nodes/{projekte}"), None).await,
        404
    );
    assert_eq!(bert.get("/api/shared").await.ok(), &json!([]));

    // Shared to view.
    let s = share(&mut anna, projekte, user(bert_id), "viewer").await;
    assert_eq!(
        s["to"],
        json!({"type": "user", "id": bert_id, "name": "bert Test"})
    );
    let shared = bert.get("/api/shared").await.ok().clone();
    assert_eq!(names(&shared), ["Projekte"]);
    assert_eq!(shared[0]["owner"], "anna Test");
    assert_eq!(shared[0]["role"], "viewer");
    let p = bert
        .get(&format!("/api/nodes/{projekte}"))
        .await
        .ok()
        .clone();
    assert_eq!(crumbs(&p), ["Projekte"], "nichts über dem geteilten Ordner");
    assert_eq!(
        (p["role"].as_str(), p["shared"].as_bool()),
        (Some("viewer"), Some(true))
    );
    let t = bert.get(&format!("/api/nodes/{tief}")).await.ok().clone();
    assert_eq!(crumbs(&t), ["Projekte", "Unter", "Tief.txt"]);
    assert_eq!(
        names(
            bert.get(&format!("/api/nodes/{projekte}/children"))
                .await
                .ok()
        ),
        ["Plan.txt", "Unter"]
    );
    let raw = bert
        .get_raw(&format!("/api/nodes/{}/content", plan.id), &[])
        .await;
    assert_eq!(
        (raw.status.as_u16(), raw.bytes.as_slice()),
        (200, &b"Plan"[..])
    );
    assert_eq!(
        status(
            &mut bert,
            "GET",
            &format!("/api/nodes/{}/versions", plan.id),
            None
        )
        .await,
        200
    );
    // Outside the share: not found, wherever bert looks.
    for path in [
        format!("/api/nodes/{privat}"),
        format!("/api/nodes/{geheim}/content"),
        format!("/api/nodes/{anna_node}"),
        format!("/api/nodes/{anna_node}/children"),
        format!("/api/roots/{anna_root}/trash"),
        format!("/api/sync/changes?root={anna_root}"),
    ] {
        assert_eq!(status(&mut bert, "GET", &path, None).await, 404, "{path}");
    }
    // Viewing only: no changes.
    assert_eq!(upload(&mut bert, projekte, "neu.txt", b"x").await, 403);
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/nodes/{projekte}/folders"),
            Some(json!({"name": "x"}))
        )
        .await,
        403
    );
    assert_eq!(
        status(
            &mut bert,
            "PATCH",
            &format!("/api/nodes/{}", plan.id),
            Some(json!({"name": "x.txt"}))
        )
        .await,
        403
    );
    assert_eq!(
        status(
            &mut bert,
            "DELETE",
            &format!("/api/nodes/{}", plan.id),
            None
        )
        .await,
        403
    );
    let r = bert
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", plan.id, plan.rev),
            b"neu".to_vec(),
        )
        .await;
    assert_eq!(r.status, 403);
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/nodes/{projekte}/shares"),
            Some(json!({"type": "user", "id": carla_id, "role": "viewer"}))
        )
        .await,
        403
    );

    // To edit: change inside, but not the shared folder itself, and nothing outside.
    share(&mut anna, projekte, user(bert_id), "editor").await;
    assert_eq!(
        upload(&mut bert, projekte, "Von Bert.txt", b"Bert").await,
        201
    );
    assert!(dir.join("Projekte/Von Bert.txt").exists());
    assert_eq!(
        status(
            &mut bert,
            "PATCH",
            &format!("/api/nodes/{}", plan.id),
            Some(json!({"name": "Plan 2.txt"}))
        )
        .await,
        200
    );
    assert_eq!(
        status(
            &mut bert,
            "PATCH",
            &format!("/api/nodes/{}", plan.id),
            Some(json!({"parent_id": unter}))
        )
        .await,
        200
    );
    // Into a folder bert cannot see: as if it did not exist.
    assert_eq!(
        status(
            &mut bert,
            "PATCH",
            &format!("/api/nodes/{}", plan.id),
            Some(json!({"parent_id": privat}))
        )
        .await,
        404
    );
    assert!(dir.join("Projekte/Unter/Plan 2.txt").exists());
    assert_eq!(
        status(
            &mut bert,
            "PATCH",
            &format!("/api/nodes/{projekte}"),
            Some(json!({"name": "Weg"}))
        )
        .await,
        403
    );
    assert_eq!(
        status(&mut bert, "DELETE", &format!("/api/nodes/{projekte}"), None).await,
        403
    );
    // Deleting goes to anna's trash; bert may undo it, but never destroy anything for good.
    assert_eq!(
        status(&mut bert, "DELETE", &format!("/api/nodes/{tief}"), None).await,
        204
    );
    assert_eq!(
        status(&mut bert, "DELETE", &format!("/api/trash/{tief}"), None).await,
        403
    );
    assert_eq!(
        status(
            &mut bert,
            "GET",
            &format!("/api/roots/{anna_root}/trash"),
            None
        )
        .await,
        404
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/trash/{tief}/restore"),
            Some(json!({}))
        )
        .await,
        200
    );
    assert!(dir.join("Projekte/Unter/Tief.txt").exists());
    // Is its folder gone meanwhile, it could only come back at the top of anna's root: not for
    // bert.
    assert_eq!(
        status(&mut bert, "DELETE", &format!("/api/nodes/{tief}"), None).await,
        204
    );
    assert_eq!(
        status(&mut anna, "DELETE", &format!("/api/nodes/{unter}"), None).await,
        204
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/trash/{tief}/restore"),
            Some(json!({}))
        )
        .await,
        404
    );
    assert_eq!(
        status(
            &mut anna,
            "POST",
            &format!("/api/trash/{unter}/restore"),
            Some(json!({}))
        )
        .await,
        200
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/trash/{tief}/restore"),
            Some(json!({}))
        )
        .await,
        200
    );
    // Editors do not share further.
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/nodes/{unter}/shares"),
            Some(json!({"type": "user", "id": carla_id, "role": "viewer"}))
        )
        .await,
        403
    );

    // To manage: share further, but never more than one has.
    share(&mut anna, projekte, user(bert_id), "manager").await;
    let c = share(&mut bert, unter, user(carla_id), "viewer").await;
    let t = carla.get(&format!("/api/nodes/{tief}")).await.ok().clone();
    assert_eq!(crumbs(&t), ["Unter", "Tief.txt"]);
    assert_eq!(
        status(&mut carla, "GET", &format!("/api/nodes/{projekte}"), None).await,
        404
    );
    let r = bert
        .post(
            &format!("/api/nodes/{unter}/shares"),
            json!({"type": "user", "id": carla_id, "role": "owner"}),
        )
        .await;
    assert_eq!(r.status, 400);
    // Not with oneself, not with the owner, not with an end in the past.
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/nodes/{unter}/shares"),
            Some(json!({"type": "user", "id": bert_id, "role": "viewer"}))
        )
        .await,
        400
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/nodes/{unter}/shares"),
            Some(json!({"type": "user", "id": uid(&env, "anna").await, "role": "viewer"}))
        )
        .await,
        400
    );
    assert_eq!(
        status(&mut anna, "POST", &format!("/api/nodes/{unter}/shares"), Some(json!({"type": "user", "id": carla_id, "role": "viewer", "expires_at": "2020-01-01T00:00:00Z"}))).await,
        400
    );
    // A whole root is not shared, only what is in it.
    assert_eq!(
        status(
            &mut anna,
            "POST",
            &format!("/api/nodes/{anna_node}/shares"),
            Some(json!({"type": "user", "id": carla_id, "role": "viewer"}))
        )
        .await,
        400
    );

    // Expired: no access any more.
    sqlx::query("UPDATE shares SET expires_at = now() - interval '1 minute' WHERE id = $1")
        .bind(c["id"].as_i64().unwrap())
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(
        status(&mut carla, "GET", &format!("/api/nodes/{tief}"), None).await,
        404
    );
    assert_eq!(carla.get("/api/shared").await.ok(), &json!([]));

    // Ended by anna: bert loses everything at once.
    assert_eq!(
        status(
            &mut anna,
            "DELETE",
            &format!("/api/shares/{}", s["id"]),
            None
        )
        .await,
        204
    );
    assert_eq!(
        status(&mut bert, "GET", &format!("/api/nodes/{projekte}"), None).await,
        404
    );
    assert_eq!(
        status(
            &mut bert,
            "GET",
            &format!("/api/nodes/{tief}/content"),
            None
        )
        .await,
        404
    );
    assert_eq!(bert.get("/api/shared").await.ok(), &json!([]));

    // The audit log knows who shared what.
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'share_%' ORDER BY id")
            .fetch_all(&env.db.pool)
            .await
            .unwrap();
    assert!(actions.contains(&"share_created".into()) && actions.contains(&"share_removed".into()));
    env.finish().await;
}

#[tokio::test]
async fn nichts_ueber_dem_geteilten_wird_verraten() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    xlrx_server::search::start(&env.state).await.unwrap();
    let (mut anna, anna_root, _, dir) = signed_in(&env, "anna").await;
    write(&dir.join("Familie/Urlaub/Kroatien/Strand.txt"), b"Strand");
    write(&dir.join("Familie/Urlaub/Notiz Kroatien.txt"), b"Notiz");
    let home = root(&env, anna_root).await;
    roots::scan(&env.state, &home).await.unwrap();
    let urlaub = node(&env, &home, "Familie/Urlaub").await.id;
    let kroatien = node(&env, &home, "Familie/Urlaub/Kroatien").await.id;
    let strand = node(&env, &home, "Familie/Urlaub/Kroatien/Strand.txt")
        .await
        .id;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let (_, _, _, _) = signed_in(&env, "carla").await;
    share(&mut anna, urlaub, user(uid(&env, "carla").await), "viewer").await;
    share(&mut anna, kroatien, user(uid(&env, "bert").await), "viewer").await;

    let s = bert.get(&format!("/api/nodes/{strand}")).await.ok().clone();
    assert_eq!(crumbs(&s), ["Kroatien", "Strand.txt"]);
    // Who has access: only what is on the shared folder, not carla's share on "Urlaub" above.
    let a = bert
        .get(&format!("/api/nodes/{strand}/shares"))
        .await
        .ok()
        .clone();
    assert_eq!(a["owner"], "anna Test");
    assert_eq!(a["can_share"], false);
    let on: Vec<&str> = a["shares"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["node_name"].as_str().unwrap())
        .collect();
    assert_eq!(on, ["Kroatien"]);
    // Anna sees all of them, the inherited one marked.
    let a = anna
        .get(&format!("/api/nodes/{strand}/shares"))
        .await
        .ok()
        .clone();
    assert_eq!(a["shares"].as_array().unwrap().len(), 2);
    assert!(
        a["shares"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["inherited"] == true)
    );
    // Search: found, with the place starting at the share; "Notiz Kroatien" next to it is not.
    let r = bert.get("/api/search?q=kroatien").await.ok().clone();
    let found: Vec<(&str, &str)> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["name"].as_str().unwrap(), h["folder"].as_str().unwrap()))
        .collect();
    assert_eq!(found, [("Kroatien", "Geteilt")]);
    assert_eq!(r["total"], 1);
    let r = bert.get("/api/search?q=strand").await.ok().clone();
    assert_eq!(r["hits"][0]["folder"], "Geteilt/Kroatien");
    let r = bert.get("/api/search/suggest?q=Notiz").await.ok().clone();
    assert_eq!(r, json!([]));
    // In anna's own results nothing changes.
    let r = anna.get("/api/search?q=strand").await.ok().clone();
    assert_eq!(
        r["hits"][0]["folder"],
        "Meine Ablage/Familie/Urlaub/Kroatien"
    );
    // Unshared: gone from bert's search at once, without touching the index.
    let id: i64 = sqlx::query_scalar("SELECT id FROM shares WHERE node_id = $1")
        .bind(kroatien)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    anna.send("DELETE", &format!("/api/shares/{id}"), None)
        .await;
    assert_eq!(bert.get("/api/search?q=strand").await.ok()["total"], 0);
    env.state.search.get().unwrap().stop().await;
    env.finish().await;
}

#[tokio::test]
async fn gruppen_und_geteilte_ablagen() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let invite = env.invite("admin", true).await;
    let (mut admin, _, _) = setup_with_totp(&env, &invite).await;
    let (mut anna, anna_root, _, anna_dir) = signed_in(&env, "anna").await;
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    let (mut carla, _, _, _) = signed_in(&env, "carla").await;
    let (mut dora, _, _, _) = signed_in(&env, "dora").await;
    let (bert_id, carla_id, dora_id) = (
        uid(&env, "bert").await,
        uid(&env, "carla").await,
        uid(&env, "dora").await,
    );

    // Only administrators manage groups and shared roots.
    assert_eq!(
        status(&mut bert, "GET", "/api/admin/groups", None).await,
        403
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            "/api/admin/spaces",
            Some(json!({"name": "x", "path": "x"}))
        )
        .await,
        403
    );
    let g = admin
        .post(
            "/api/admin/groups",
            json!({"name": "Familie", "members": [bert_id, carla_id]}),
        )
        .await
        .ok()["id"]
        .as_i64()
        .unwrap();
    assert_eq!(
        admin
            .post(
                "/api/admin/groups",
                json!({"name": "familie", "members": []})
            )
            .await
            .status,
        409
    );

    // Shared with the group: all its members, and only while they are members.
    write(&anna_dir.join("Rezepte/Kuchen.txt"), b"Kuchen");
    let home = root(&env, anna_root).await;
    roots::scan(&env.state, &home).await.unwrap();
    let rezepte = node(&env, &home, "Rezepte").await.id;
    share(
        &mut anna,
        rezepte,
        json!({"type": "group", "id": g}),
        "viewer",
    )
    .await;
    for c in [&mut bert, &mut carla] {
        assert_eq!(names(c.get("/api/shared").await.ok()), ["Rezepte"]);
    }
    assert_eq!(
        status(&mut dora, "GET", &format!("/api/nodes/{rezepte}"), None).await,
        404
    );
    admin
        .send(
            "PUT",
            &format!("/api/admin/groups/{g}"),
            Some(json!({"name": "Familie", "members": [bert_id]})),
        )
        .await;
    assert_eq!(
        status(&mut carla, "GET", &format!("/api/nodes/{rezepte}"), None).await,
        404
    );
    assert_eq!(
        status(&mut bert, "GET", &format!("/api/nodes/{rezepte}"), None).await,
        200
    );

    // A team folder on the NAS becomes a shared root with members.
    let data = env.data_dir().to_path_buf();
    write(&data.join("Familie/Fotos/Sommer.txt"), b"Sommer");
    std::os::unix::fs::symlink(data.join("Familie"), data.join("Verknuepfung")).unwrap();
    for (path, code) in [
        ("../etc", 400),
        ("/etc", 400),
        ("Gibt es nicht", 400),
        ("Verknuepfung", 400),
        ("xlrx-state", 400),
        ("homes", 409),
        ("homes/anna/Drive/Rezepte", 409),
    ] {
        let r = admin
            .post("/api/admin/spaces", json!({"name": "Test", "path": path}))
            .await;
        assert_eq!(r.status.as_u16(), code, "{path}: {}", r.body);
    }
    let space = admin
        .post(
            "/api/admin/spaces",
            json!({"name": "Familie", "path": "Familie", "members": [
                {"type": "group", "id": g, "role": "editor"},
                {"type": "user", "id": dora_id, "role": "viewer"}
            ]}),
        )
        .await
        .ok()["id"]
        .as_i64()
        .unwrap();
    let sr = root(&env, space).await;
    roots::scan(&env.state, &sr).await.unwrap();
    let space_node = db::root_node(&env.db.pool, space)
        .await
        .unwrap()
        .unwrap()
        .id;
    let sommer = node(&env, &sr, "Fotos/Sommer.txt").await.id;
    // Members see it among their roots, with their role; others do not.
    let r = bert.get("/api/roots").await.ok().clone();
    assert_eq!(r[1]["name"], "Familie");
    assert_eq!(r[1]["role"], "editor");
    assert_eq!(dora.get("/api/roots").await.ok()[1]["role"], "viewer");
    assert_eq!(
        anna.get("/api/roots").await.ok().as_array().unwrap().len(),
        1
    );
    assert_eq!(
        status(&mut anna, "GET", &format!("/api/nodes/{sommer}"), None).await,
        404
    );
    assert_eq!(
        status(&mut admin, "GET", &format!("/api/nodes/{sommer}"), None).await,
        404
    );
    let s = bert.get(&format!("/api/nodes/{sommer}")).await.ok().clone();
    assert_eq!(crumbs(&s), ["Familie", "Fotos", "Sommer.txt"]);
    assert_eq!(s["shared"], false);
    // Editors change things, viewers only look; the trash belongs to those who may edit.
    assert_eq!(upload(&mut bert, space_node, "Neu.txt", b"neu").await, 201);
    assert!(data.join("Familie/Neu.txt").exists());
    assert_eq!(upload(&mut dora, space_node, "Dora.txt", b"d").await, 403);
    assert_eq!(
        status(&mut bert, "DELETE", &format!("/api/nodes/{sommer}"), None).await,
        204
    );
    assert_eq!(
        status(&mut dora, "GET", &format!("/api/roots/{space}/trash"), None).await,
        403
    );
    assert_eq!(
        names(bert.get(&format!("/api/roots/{space}/trash")).await.ok()),
        ["Sommer.txt"]
    );
    assert_eq!(
        status(
            &mut bert,
            "POST",
            &format!("/api/trash/{sommer}/restore"),
            Some(json!({}))
        )
        .await,
        200
    );
    // Sync clients of members can follow it.
    assert_eq!(
        status(
            &mut dora,
            "GET",
            &format!("/api/sync/changes?root={space}"),
            None
        )
        .await,
        200
    );
    assert_eq!(
        status(
            &mut anna,
            "GET",
            &format!("/api/sync/changes?root={space}"),
            None
        )
        .await,
        404
    );
    // The group removed: its members are out.
    assert_eq!(
        status(
            &mut admin,
            "DELETE",
            &format!("/api/admin/groups/{g}"),
            None
        )
        .await,
        204
    );
    assert_eq!(
        status(&mut bert, "GET", &format!("/api/nodes/{sommer}"), None).await,
        404
    );
    assert_eq!(
        status(&mut dora, "GET", &format!("/api/nodes/{sommer}"), None).await,
        200
    );
    let spaces = admin.get("/api/admin/spaces").await.ok().clone();
    assert_eq!(spaces[0]["members"].as_array().unwrap().len(), 1);
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
async fn live_nur_aus_dem_geteilten() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut anna, anna_root, _, dir) = signed_in(&env, "anna").await;
    write(&dir.join("Projekte/a.txt"), b"a");
    write(&dir.join("Privat/b.txt"), b"b");
    let home = root(&env, anna_root).await;
    roots::scan(&env.state, &home).await.unwrap();
    let projekte = node(&env, &home, "Projekte").await.id;
    let privat = node(&env, &home, "Privat").await.id;
    let (bert, _, _, _) = signed_in(&env, "bert").await;
    share(&mut anna, projekte, user(uid(&env, "bert").await), "viewer").await;
    let req = Request::builder()
        .uri("/api/sync/notify")
        .header(
            header::COOKIE,
            format!("xlrx_session={}", bert.cookie.clone().unwrap()),
        )
        .body(Body::empty())
        .unwrap();
    let mut stream = env.app.clone().oneshot(req).await.unwrap().into_body();

    // Changes elsewhere in anna's root are none of bert's business.
    anna.post(
        &format!("/api/nodes/{privat}/folders"),
        json!({"name": "Neu"}),
    )
    .await
    .ok();
    let r = tokio::time::timeout(Duration::from_millis(1500), next_change(&mut stream)).await;
    assert!(
        r.is_err(),
        "Änderung außerhalb der Freigabe gemeldet: {r:?}"
    );
    // Inside the shared folder: bert hears about it.
    anna.post(
        &format!("/api/nodes/{projekte}/folders"),
        json!({"name": "Neu"}),
    )
    .await
    .ok();
    let ev = next_change(&mut stream).await;
    assert_eq!(ev[0]["root"], anna_root);
    env.finish().await;
}

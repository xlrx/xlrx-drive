//! Uploads in parts: any order, retries, damaged and overlapping parts, resuming, putting in place
//! (new file, replace, content for sync), interrupted commits, cleanup.

mod common;

use std::fs;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use xlrx_chunk::Chunker;
use xlrx_server::api::uploads;

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

const MIB: usize = 1024 * 1024;

/// Reproducible bytes that do not compress or repeat.
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn content_hash(bytes: &[u8]) -> String {
    let h = Chunker::new().digest_slice(bytes).content.hash;
    h.0.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

async fn start(c: &mut Client, size: usize, target: Value) -> String {
    let r = c
        .post(
            "/api/uploads",
            json!({"size": size, "target": target, "mtime_ms": 1_700_000_000_000i64}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

async fn part(c: &mut Client, id: &str, offset: usize, bytes: &[u8]) -> Resp {
    let sum = sha(bytes);
    c.send_bytes_with(
        "PUT",
        &format!("/api/uploads/{id}/parts?offset={offset}"),
        bytes.to_vec(),
        &[("x-content-sha256", &sum)],
    )
    .await
}

fn received(r: &Resp) -> Value {
    r.ok()["received"].clone()
}

#[tokio::test]
async fn teile_in_beliebiger_reihenfolge_und_wiederaufnahme() {
    let env = env_or_skip!();
    let (mut c, _, root_node, dir) = signed_in(&env, "anna").await;
    let bytes = data(20 * MIB + 123, 1);
    let (a, rest) = bytes.split_at(8 * MIB);
    let (b, tail) = rest.split_at(8 * MIB);
    let id = start(
        &mut c,
        bytes.len(),
        json!({"kind": "new", "parent_id": root_node, "name": "Film.mp4"}),
    )
    .await;

    // Second part first, then the first: one range.
    assert_eq!(
        received(&part(&mut c, &id, 8 * MIB, b).await),
        json!([[8 * MIB, 16 * MIB]])
    );
    assert_eq!(
        received(&part(&mut c, &id, 0, a).await),
        json!([[0, 16 * MIB]])
    );
    // A retry of a part that arrived: confirmed again, nothing changes.
    assert_eq!(
        received(&part(&mut c, &id, 0, a).await),
        json!([[0, 16 * MIB]])
    );

    // Inside what arrived: confirmed without writing (nothing that arrived is ever overwritten).
    assert_eq!(
        received(&part(&mut c, &id, 4 * MIB, &tail[..10]).await),
        json!([[0, 16 * MIB]])
    );
    let first = c.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(received(&first), json!([[0, 16 * MIB]]));

    // Refused: partly overlapping, damaged, past the end, too large, empty.
    let r = part(&mut c, &id, 16 * MIB - 5, &tail[..10]).await;
    assert_eq!(r.status, 409);
    let r = c
        .send_bytes_with(
            "PUT",
            &format!("/api/uploads/{id}/parts?offset={}", 16 * MIB),
            tail.to_vec(),
            &[("x-content-sha256", &sha(b"etwas anderes"))],
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
    let r = part(&mut c, &id, 16 * MIB + 1, tail).await;
    assert_eq!(r.status, 400);
    let r = part(&mut c, &id, 16 * MIB, &data(8 * MIB + 1, 2)).await;
    assert_eq!(r.status, 400);
    let r = c
        .send_bytes("PUT", &format!("/api/uploads/{id}/parts?offset=0"), vec![])
        .await;
    assert_eq!(r.status, 400);

    // Not complete yet: committing is refused, the parts stay.
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.status, 409);
    // Resuming: the status tells what arrived.
    let status = c.get(&format!("/api/uploads/{id}")).await;
    assert_eq!(received(&status), json!([[0, 16 * MIB]]));
    assert_eq!(status.ok()["state"], "open");
    // Someone else sees nothing of it.
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(bert.get(&format!("/api/uploads/{id}")).await.status, 404);
    assert_eq!(part(&mut bert, &id, 16 * MIB, tail).await.status, 404);

    // Last part without a checksum (allowed), then put in place with the content hash.
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/uploads/{id}/parts?offset={}", 16 * MIB),
            tail.to_vec(),
        )
        .await;
    assert_eq!(received(&r), json!([[0, bytes.len()]]));
    let done = c
        .post(
            &format!("/api/uploads/{id}/commit"),
            json!({"hash": content_hash(&bytes)}),
        )
        .await;
    let node = done.ok().clone();
    assert_eq!(
        (node["name"].as_str(), node["size"].as_u64()),
        (Some("Film.mp4"), Some(bytes.len() as u64))
    );
    assert!(
        fs::read(dir.join("Film.mp4")).unwrap() == bytes,
        "Inhalt auf dem NAS"
    );
    // Repeating the commit gives the same answer; parts are no longer accepted.
    let again = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(again.ok(), &node);
    assert_eq!(part(&mut c, &id, 0, a).await.status, 409);
    env.finish().await;
}

#[tokio::test]
async fn falscher_hash_abbruch_und_konflikte() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "carla").await;
    let new = |name: &str| json!({"kind": "new", "parent_id": root_node, "name": name});

    // Wrong hash: nothing is created, the upload is gone.
    let bytes = data(1000, 3);
    let id = start(&mut c, bytes.len(), new("a.bin")).await;
    part(&mut c, &id, 0, &bytes).await.ok();
    let r = c
        .post(
            &format!("/api/uploads/{id}/commit"),
            json!({"hash": content_hash(b"anders")}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert!(!dir.join("a.bin").exists());
    assert_eq!(c.get(&format!("/api/uploads/{id}")).await.status, 404);

    // Given up: the parts are removed.
    let id = start(&mut c, bytes.len(), new("b.bin")).await;
    part(&mut c, &id, 0, &bytes[..500]).await.ok();
    let file = env
        .data_dir()
        .join("xlrx-state/store/uploads")
        .join(id.replace('-', ""));
    assert!(file.is_file());
    assert_eq!(
        c.send("DELETE", &format!("/api/uploads/{id}"), None)
            .await
            .status,
        204
    );
    assert!(!file.exists());
    assert_eq!(c.get(&format!("/api/uploads/{id}")).await.status, 404);

    // Name taken from the start: refused before anything is sent.
    write(&dir.join("da.txt"), b"schon da");
    c.post(&format!("/api/roots/{root_id}/scan"), json!({}))
        .await
        .ok();
    let r = c
        .post("/api/uploads", json!({"size": 3, "target": new("da.txt")}))
        .await;
    assert_eq!(r.status, 409);

    // Name taken while the upload ran: the parts stay, the client decides (keep both).
    let id = start(&mut c, bytes.len(), new("neu.bin")).await;
    part(&mut c, &id, 0, &bytes).await.ok();
    c.send_bytes(
        "POST",
        &format!("/api/nodes/{root_node}/files?name=neu.bin"),
        b"zuerst".to_vec(),
    )
    .await
    .ok();
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(
        c.get(&format!("/api/uploads/{id}")).await.ok()["state"],
        "open"
    );
    let kept = c
        .post(
            &format!("/api/uploads/{id}/commit"),
            json!({"keep_both": true}),
        )
        .await
        .ok()
        .clone();
    assert_ne!(kept["name"], "neu.bin");
    assert_eq!(fs::read(dir.join("neu.bin")).unwrap(), b"zuerst");
    assert!(fs::read(dir.join(kept["name"].as_str().unwrap())).unwrap() == bytes);

    // Replacing a file that changed meanwhile: refused, the parts stay.
    let file = find(
        c.get(&format!("/api/nodes/{root_node}/children"))
            .await
            .ok(),
        "neu.bin",
    )
    .clone();
    let id = start(
        &mut c,
        bytes.len(),
        json!({"kind": "replace", "node_id": file["id"], "base_rev": file["rev"]}),
    )
    .await;
    part(&mut c, &id, 0, &bytes).await.ok();
    c.send_bytes(
        "PUT",
        &format!("/api/nodes/{}/content?base_rev={}", file["id"], file["rev"]),
        b"dazwischen".to_vec(),
    )
    .await
    .ok();
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(
        c.get(&format!("/api/uploads/{id}")).await.ok()["state"],
        "open"
    );
    assert_eq!(fs::read(dir.join("neu.bin")).unwrap(), b"dazwischen");

    // Replacing on the right revision.
    let file = find(
        c.get(&format!("/api/nodes/{root_node}/children"))
            .await
            .ok(),
        "neu.bin",
    )
    .clone();
    let id = start(
        &mut c,
        bytes.len(),
        json!({"kind": "replace", "node_id": file["id"], "base_rev": file["rev"]}),
    )
    .await;
    part(&mut c, &id, 0, &bytes).await.ok();
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.ok()["id"], file["id"]);
    assert!(fs::read(dir.join("neu.bin")).unwrap() == bytes);
    let versions = c.get(&format!("/api/nodes/{}/versions", file["id"])).await;
    assert!(
        versions.ok().as_array().unwrap().len() >= 2,
        "alte Fassungen bleiben"
    );
    env.finish().await;
}

#[tokio::test]
async fn inhalt_fuer_den_sync_und_leere_datei() {
    let env = env_or_skip!();
    let (mut c, _, root_node, dir) = signed_in(&env, "dora").await;
    let bytes = data(9 * MIB, 4);
    let hash = content_hash(&bytes);
    let id = start(&mut c, bytes.len(), json!({"kind": "content"})).await;
    part(&mut c, &id, 0, &bytes[..8 * MIB]).await.ok();
    part(&mut c, &id, 8 * MIB, &bytes[8 * MIB..]).await.ok();
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({"hash": hash}))
        .await;
    assert_eq!(r.ok()["hash"], hash);
    let available = c.get(&format!("/api/sync/content/{hash}")).await;
    assert_eq!(available.ok()["available"], true);

    // An empty file needs no parts.
    let id = start(
        &mut c,
        0,
        json!({"kind": "new", "parent_id": root_node, "name": "leer.txt"}),
    )
    .await;
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.ok()["size"], 0);
    assert_eq!(fs::read(dir.join("leer.txt")).unwrap(), b"");
    env.finish().await;
}

#[tokio::test]
async fn unterbrochenes_speichern_und_aufraeumen() {
    let env = env_or_skip!();
    let (mut c, _, root_node, dir) = signed_in(&env, "emil").await;
    let bytes = data(5000, 5);
    let new = |name: &str| json!({"kind": "new", "parent_id": root_node, "name": name});
    let set_state = |id: String, state: &'static str| {
        let db = env.state.db.clone();
        async move {
            sqlx::query("UPDATE uploads SET state = $2 WHERE id = $1::uuid")
                .bind(id)
                .bind(state)
                .execute(&db)
                .await
                .unwrap();
        }
    };

    // Interrupted before anything was written (its file is still there): open again after a
    // restart, and the commit works.
    let id = start(&mut c, bytes.len(), new("x.bin")).await;
    part(&mut c, &id, 0, &bytes).await.ok();
    set_state(id.clone(), "committing").await;
    let r = c
        .post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await;
    assert_eq!(r.status, 409, "Ergebnis unklar, solange es läuft");
    uploads::recover(&env.state).await.unwrap();
    assert_eq!(
        c.get(&format!("/api/uploads/{id}")).await.ok()["state"],
        "open"
    );
    c.post(&format!("/api/uploads/{id}/commit"), json!({}))
        .await
        .ok();
    assert!(fs::read(dir.join("x.bin")).unwrap() == bytes);

    // Without its file the outcome stays unclear (recovery took the write over).
    let id = start(&mut c, bytes.len(), new("y.bin")).await;
    part(&mut c, &id, 0, &bytes).await.ok();
    set_state(id.clone(), "committing").await;
    let uploads_dir = env.data_dir().join("xlrx-state/store/uploads");
    fs::remove_file(uploads_dir.join(id.replace('-', ""))).unwrap();
    uploads::recover(&env.state).await.unwrap();
    assert_eq!(
        c.get(&format!("/api/uploads/{id}")).await.ok()["state"],
        "committing"
    );
    // It cannot be given up either while unclear; cleanup takes it later.
    assert_eq!(
        c.send("DELETE", &format!("/api/uploads/{id}"), None)
            .await
            .status,
        404
    );

    // Cleanup: uploads idle for a day, and old files without an upload.
    let idle = start(&mut c, bytes.len(), new("z.bin")).await;
    part(&mut c, &idle, 0, &bytes[..100]).await.ok();
    sqlx::query("UPDATE uploads SET updated_at = now() - interval '25 hours' WHERE id = $1::uuid")
        .bind(&idle)
        .execute(&env.state.db)
        .await
        .unwrap();
    let active = start(&mut c, bytes.len(), new("w.bin")).await;
    let orphan = uploads_dir.join(uuid::Uuid::new_v4().simple().to_string());
    fs::write(&orphan, b"alt").unwrap();
    fs::File::options()
        .write(true)
        .open(&orphan)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(7200))
        .unwrap();
    let fresh = uploads_dir.join(uuid::Uuid::new_v4().simple().to_string());
    fs::write(&fresh, b"neu").unwrap();
    uploads::housekeeping(&env.state).await.unwrap();
    assert_eq!(c.get(&format!("/api/uploads/{idle}")).await.status, 404);
    assert!(!uploads_dir.join(idle.replace('-', "")).exists());
    assert!(!orphan.exists());
    assert!(
        fresh.exists(),
        "frische Dateien können zu einem neuen Upload gehören"
    );
    assert_eq!(
        c.get(&format!("/api/uploads/{active}")).await.ok()["state"],
        "open"
    );
    assert!(uploads_dir.join(active.replace('-', "")).exists());
    env.finish().await;
}

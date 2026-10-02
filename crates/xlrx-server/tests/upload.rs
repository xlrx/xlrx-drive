//! Uploads, replacing content, versions, and recovery after a crash during an upload.

mod common;

use std::fs;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_chunk::Chunker;
use xlrx_server::files::{db, ops, roots};

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

fn hash(data: &[u8]) -> [u8; 32] {
    Chunker::new().digest_slice(data).content.hash.0
}

fn hex(h: &[u8]) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

fn id(v: &Value) -> i64 {
    v["id"].as_i64().unwrap()
}

/// Pseudo-random bytes (several chunks).
fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

async fn versions(c: &mut Client, node: i64) -> Vec<Value> {
    c.get(&format!("/api/nodes/{node}/versions"))
        .await
        .ok()
        .as_array()
        .unwrap()
        .clone()
}

fn staging_empty(env: &Env) -> bool {
    fs::read_dir(env.data_dir().join("xlrx-state/store/staging"))
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

#[tokio::test]
async fn hochladen() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "anna").await;
    let big = data(5_000_000, 1);

    // 2021-03-04 05:06:07.890 UTC
    let r = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{root_node}/files?name=Video%20Urlaub.mp4&mtime_ms=1614834367890&size=5000000"),
            big.clone(),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["name"], "Video Urlaub.mp4");
    assert_eq!(r.body["size"], 5_000_000);
    assert_eq!(fs::read(dir.join("Video Urlaub.mp4")).unwrap(), big);
    let node = db::node_by_id(&env.db.pool, id(&r.body))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(node.content_hash.as_deref(), Some(&hash(&big)[..]));
    let mtime = fs::metadata(dir.join("Video Urlaub.mp4"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        mtime
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        1_614_834_367_890
    );
    assert_eq!(journal_ops(&env, node.id).await, ["create"]);
    assert!(staging_empty(&env));

    // Taken (also ignoring case): refused – or "Name (1).ext" on request.
    for name in ["Video%20Urlaub.mp4", "video%20urlaub.MP4"] {
        let r = c
            .send_bytes(
                "POST",
                &format!("/api/nodes/{root_node}/files?name={name}"),
                b"x".to_vec(),
            )
            .await;
        assert_eq!(r.status, 409, "{name}");
    }
    let r = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{root_node}/files?name=Video%20Urlaub.mp4&keep_both=true"),
            b"zweites".to_vec(),
        )
        .await;
    assert_eq!(r.ok()["name"], "Video Urlaub (1).mp4");
    assert_eq!(
        fs::read(dir.join("Video Urlaub.mp4")).unwrap(),
        big,
        "original untouched"
    );

    // Incomplete or oversized uploads leave nothing behind.
    for size in [10, 2] {
        let r = c
            .send_bytes(
                "POST",
                &format!("/api/nodes/{root_node}/files?name=kaputt.bin&size={size}"),
                b"12345".to_vec(),
            )
            .await;
        assert_eq!(r.status, 400, "{}", r.body);
    }
    assert!(!dir.join("kaputt.bin").exists());
    assert!(staging_empty(&env));
    // Empty files are files too.
    let r = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{root_node}/files?name=leer.txt"),
            vec![],
        )
        .await;
    assert_eq!(r.ok()["size"], 0);
    // Bad names and targets.
    for (parent, name, status) in [
        (root_node, ".DS_Store", 400),
        (root_node, "a%2Fb", 400),
        (999_999, "x.txt", 404),
        (node.id, "x.txt", 400),
    ] {
        let r = c
            .send_bytes(
                "POST",
                &format!("/api/nodes/{parent}/files?name={name}"),
                b"x".to_vec(),
            )
            .await;
        assert_eq!(r.status, status, "{parent}/{name}: {}", r.body);
    }

    // The scan agrees with what was uploaded.
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    let report = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(report.changes(), 0, "{report:?}");

    // Others cannot upload here.
    let (mut other, _, _, _) = signed_in(&env, "bert").await;
    let r = other
        .send_bytes(
            "POST",
            &format!("/api/nodes/{root_node}/files?name=x.txt"),
            b"x".to_vec(),
        )
        .await;
    assert_eq!(r.status, 404);
    env.finish().await;
}

#[tokio::test]
async fn ersetzen_und_versionen() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "carla").await;
    let r = c
        .send_bytes(
            "POST",
            &format!("/api/nodes/{root_node}/files?name=Bericht.txt"),
            b"Fassung 1".to_vec(),
        )
        .await;
    let file = r.ok().clone();
    let path = dir.join("Bericht.txt");

    // Replace: new content, the old one becomes a version.
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", id(&file), file["rev"]),
            b"Fassung 2, laenger".to_vec(),
        )
        .await;
    let v2 = r.ok().clone();
    assert_eq!(v2["id"], file["id"]);
    assert!(v2["rev"].as_i64() > file["rev"].as_i64());
    assert_eq!(fs::read_to_string(&path).unwrap(), "Fassung 2, laenger");
    let list = versions(&mut c, id(&file)).await;
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["rev"], file["rev"]);
    assert_eq!(list[0]["size"], 9);
    assert_eq!(list[0]["created_by"], "carla Test");
    let old = c
        .get_raw(&format!("/api/versions/{}/content", id(&list[0])), &[])
        .await;
    assert_eq!(old.status, 200);
    assert_eq!(old.bytes, b"Fassung 1");
    assert!(old.header("content-disposition").starts_with("attachment;"));
    assert_eq!(journal_ops(&env, id(&file)).await, ["create", "update"]);

    // Based on an old revision: refused, nothing lost.
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", id(&file), file["rev"]),
            b"veraltet".to_vec(),
        )
        .await;
    assert_eq!(r.status, 409);
    assert_eq!(fs::read_to_string(&path).unwrap(), "Fassung 2, laenger");

    // Changed over SMB in place (not scanned yet): refused, the SMB change stays and is picked
    // up; the next upload keeps it as a version.
    fs::write(&path, "per SMB geändert").unwrap();
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", id(&file), v2["rev"]),
            b"Fassung 3".to_vec(),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(fs::read_to_string(&path).unwrap(), "per SMB geändert");
    let now = c
        .get(&format!("/api/nodes/{}", id(&file)))
        .await
        .ok()
        .clone();
    assert_ne!(now["rev"], v2["rev"], "the scan recorded the SMB change");
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", id(&file), now["rev"]),
            b"Fassung 3".to_vec(),
        )
        .await;
    r.ok();
    let list = versions(&mut c, id(&file)).await;
    assert_eq!(list.len(), 2);
    let smb = c
        .get_raw(&format!("/api/versions/{}/content", id(&list[0])), &[])
        .await;
    assert_eq!(smb.bytes, "per SMB geändert".as_bytes());
    // (The version from before the SMB change was overwritten in place by SMB: only xlrx's own
    // writes keep versions.)

    // Restore the first version: the current content becomes a version too.
    let first = list.last().unwrap();
    let r = c
        .post(&format!("/api/versions/{}/restore", id(first)), json!({}))
        .await;
    r.ok();
    assert_eq!(fs::read_to_string(&path).unwrap(), "Fassung 1");
    let list = versions(&mut c, id(&file)).await;
    assert_eq!(list.len(), 3);
    // Same content is stored once (content-addressed).
    let mut stored = Vec::new();
    let mut stack = vec![env.data_dir().join("xlrx-state/store/versions")];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                stored.push(p);
            }
        }
    }
    assert_eq!(stored.len(), 3, "Fassung 1, SMB, Fassung 3: {stored:?}");

    // The scan agrees.
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(roots::scan(&env.state, &root).await.unwrap().changes(), 0);

    // Others see no versions.
    let (mut other, _, _, _) = signed_in(&env, "dora").await;
    assert_eq!(
        other
            .get(&format!("/api/nodes/{}/versions", id(&file)))
            .await
            .status,
        404
    );
    assert_eq!(
        other
            .get_raw(&format!("/api/versions/{}/content", id(first)), &[])
            .await
            .status,
        404
    );
    assert_eq!(
        other
            .post(&format!("/api/versions/{}/restore", id(first)), json!({}))
            .await
            .status,
        404
    );
    let r = other
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev=1", id(&file)),
            b"x".to_vec(),
        )
        .await;
    assert_eq!(r.status, 404);
    env.finish().await;
}

#[tokio::test]
async fn absturz_beim_hochladen() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "emil").await;
    let user = root.owner_user_id.unwrap();
    for n in ["a", "b", "c"] {
        write(&dir.join(format!("{n}.txt")), format!("alt {n}").as_bytes());
    }
    roots::scan(&env.state, &root).await.unwrap();
    let state = env.data_dir().join("xlrx-state");
    for d in ["store/staging", "store/versions"] {
        fs::create_dir_all(state.join(d)).unwrap();
    }
    let rel = |n: &str| format!("homes/emil/Drive/{n}");
    let intent = |n: &str, node: &db::NodeRow, temp: &str| {
        let old_hash = hash(format!("alt {n}").as_bytes());
        json!({"op": "upload", "root_id": root.id, "actor": user,
            "staging": format!("store/staging/{n}"), "temp": rel(temp), "target": rel(&format!("{n}.txt")),
            "hash": hex(&hash(format!("neu {n}").as_bytes())),
            "old": {"node_id": node.id, "rev": node.rev, "hash": hex(&old_hash), "size": 5,
                    "mtime": null, "store_path": format!("store/versions/{}", hex(&old_hash))}})
    };
    let insert = |payload: Value| {
        let pool = env.db.pool.clone();
        async move {
            sqlx::query("INSERT INTO pending_ops (kind, payload) VALUES ('upload', $1)")
                .bind(payload)
                .execute(&pool)
                .await
                .unwrap();
        }
    };
    // a: new content prepared next to the target, not swapped in yet.
    let a = node(&env, &root, "a.txt").await;
    fs::write(state.join("store/staging/a"), "neu a").unwrap();
    fs::write(dir.join(".xlrx-srv-a"), "neu a").unwrap();
    insert(intent("a", &a, ".xlrx-srv-a")).await;
    // b: swapped in, the old file still under the temp name, its version kept.
    let b = node(&env, &root, "b.txt").await;
    fs::write(
        state.join(format!("store/versions/{}", hex(&hash(b"alt b")))),
        "alt b",
    )
    .unwrap();
    fs::write(dir.join("b.txt"), "neu b").unwrap();
    fs::write(dir.join(".xlrx-srv-b"), "alt b").unwrap();
    insert(intent("b", &b, ".xlrx-srv-b")).await;
    // c: something unknown under the temp name (never removed).
    let c = node(&env, &root, "c.txt").await;
    fs::write(dir.join(".xlrx-srv-c"), "unbekannt").unwrap();
    insert(intent("c", &c, ".xlrx-srv-c")).await;

    ops::recover(&env.state, None).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM pending_ops")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(!dir.join(".xlrx-srv-a").exists() && !dir.join(".xlrx-srv-b").exists());
    assert_eq!(fs::read_to_string(dir.join("a.txt")).unwrap(), "alt a");
    assert_eq!(
        fs::read_to_string(dir.join("c (gerettet).txt")).unwrap(),
        "unbekannt"
    );
    assert!(staging_empty(&env));
    let versions_b: i64 = sqlx::query_scalar("SELECT count(*) FROM versions WHERE node_id = $1")
        .bind(b.id)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(versions_b, 1);
    // The scan brings the nodes up to date.
    roots::scan(&env.state, &root).await.unwrap();
    let b_now = node(&env, &root, "b.txt").await;
    assert_eq!(b_now.id, b.id);
    assert_eq!(b_now.content_hash.as_deref(), Some(&hash(b"neu b")[..]));
    assert_eq!(
        names(&env, &root).await,
        ["a.txt", "b.txt", "c (gerettet).txt", "c.txt"]
    );
    env.finish().await;
}

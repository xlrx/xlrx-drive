//! Operations of the sync engine against the real server: the same semantics as the simulator's
//! server (preconditions, rejections, conflict copies), executed once per operation id.

mod common;

use std::fs;

use common::files::*;
use common::*;
use serde_json::json;
use xlrx_chunk::Chunker;
use xlrx_proto::{ContentHash, FileContent, Name, NodeId, Rev};
use xlrx_server::files::{db, roots};
use xlrx_sync::{Fingerprint, LocalId, Origin, Reject, RemoteOp, RemoteResult};

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

const DEVICE: &str = "Klaus' MacBook";
/// The device name as a path segment.
const DEVICE_URL: &str = "Klaus%27%20MacBook";

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn content(data: &[u8]) -> FileContent {
    Chunker::new().digest_slice(data).content
}

fn hex(h: &ContentHash) -> String {
    h.0.iter().map(|b| format!("{b:02x}")).collect()
}

const FP: Fingerprint = Fingerprint {
    size: 0,
    mtime_ns: 0,
    ctime_ns: 0,
};

async fn op(c: &mut Client, op_id: u64, op: &RemoteOp) -> RemoteResult {
    let r = c
        .post(
            "/api/sync/ops",
            json!({ "device": DEVICE, "op_id": op_id, "op": serde_json::to_value(op).unwrap() }),
        )
        .await;
    serde_json::from_value(r.ok().clone()).unwrap()
}

async fn upload(c: &mut Client, data: &[u8]) -> FileContent {
    let fc = content(data);
    let r = c
        .send_bytes(
            "PUT",
            &format!("/api/sync/content/{}", hex(&fc.hash)),
            data.to_vec(),
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    fc
}

fn node_of(r: &RemoteResult) -> NodeId {
    match r {
        RemoteResult::Created { node, .. } | RemoteResult::Conflict { node, .. } => *node,
        other => panic!("kein neuer Knoten: {other:?}"),
    }
}

fn rev_of(r: &RemoteResult) -> Rev {
    match r {
        RemoteResult::Created { rev, .. } | RemoteResult::Updated { rev, .. } => *rev,
        other => panic!("keine Revision: {other:?}"),
    }
}

#[tokio::test]
async fn operationen_wie_im_simulator() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "klaus").await;
    let root = NodeId(root_node as u64);
    let mkdir = |parent: NodeId, n: &str| RemoteOp::CreateDir {
        parent,
        name: name(n),
        source: LocalId(1),
        origin: Origin::New,
    };

    // Folders: created once per operation id, names stay unique.
    let projekte = op(&mut c, 1, &mkdir(root, "Projekte")).await;
    assert_eq!(
        op(&mut c, 1, &mkdir(root, "Projekte")).await,
        projekte,
        "repeated: same answer"
    );
    let p = node_of(&projekte);
    assert!(dir.join("Projekte").is_dir());
    assert_eq!(
        op(&mut c, 2, &mkdir(root, "projekte")).await,
        RemoteResult::Rejected(Reject::NameTaken)
    );
    assert_eq!(
        op(&mut c, 3, &mkdir(NodeId(999_999), "x")).await,
        RemoteResult::Rejected(Reject::ParentGone)
    );
    let r = c.get(&format!("/api/sync/ops/{DEVICE_URL}/1")).await;
    assert_eq!(
        serde_json::from_value::<RemoteResult>(r.ok().clone()).unwrap(),
        projekte
    );
    assert_eq!(
        c.get(&format!("/api/sync/ops/{DEVICE_URL}/77"))
            .await
            .status,
        404
    );

    // Files: the content comes first (or is already there).
    let plan_v1 = content(b"Plan v1");
    let create = |parent: NodeId, n: &str, fc: FileContent| RemoteOp::CreateFile {
        parent,
        name: name(n),
        content: fc,
        source: LocalId(2),
        fp: FP,
        origin: Origin::New,
    };
    let missing = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 10, "op": serde_json::to_value(create(p, "plan.txt", plan_v1)).unwrap()}),
        )
        .await;
    assert_eq!(missing.status, 409);
    assert_eq!(missing.body["reason"], "content_missing");
    let wrong = c
        .send_bytes(
            "PUT",
            &format!("/api/sync/content/{}", hex(&plan_v1.hash)),
            b"anders".to_vec(),
        )
        .await;
    assert_eq!(wrong.status, 400);
    upload(&mut c, b"Plan v1").await;
    let plan = op(&mut c, 10, &create(p, "plan.txt", plan_v1)).await;
    let plan_id = node_of(&plan);
    assert_eq!(fs::read(dir.join("Projekte/plan.txt")).unwrap(), b"Plan v1");
    // The same content elsewhere: taken from the existing file, nothing to upload.
    let avail = c
        .get(&format!("/api/sync/content/{}", hex(&plan_v1.hash)))
        .await;
    assert_eq!(avail.ok()["available"], true);
    let copy = op(&mut c, 11, &create(root, "plan kopie.txt", plan_v1)).await;
    assert!(matches!(copy, RemoteResult::Created { .. }));
    assert_eq!(fs::read(dir.join("plan kopie.txt")).unwrap(), b"Plan v1");
    let unknown = content(b"nie gesehen");
    let avail = c
        .get(&format!("/api/sync/content/{}", hex(&unknown.hash)))
        .await;
    assert_eq!(avail.ok()["available"], false);

    // New content on the current revision: updated, the old one kept as a version.
    let v2 = upload(&mut c, b"Plan v2").await;
    let up = |base: Rev, fc: FileContent| RemoteOp::Upload {
        node: plan_id,
        base_rev: base,
        content: fc,
        source: LocalId(2),
        fp: FP,
    };
    let updated = op(&mut c, 12, &up(rev_of(&plan), v2)).await;
    let rev2 = rev_of(&updated);
    assert_eq!(fs::read(dir.join("Projekte/plan.txt")).unwrap(), b"Plan v2");
    assert_eq!(
        c.get(&format!("/api/nodes/{}/versions", plan_id.0))
            .await
            .ok()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // On an outdated revision: stored next to it as a conflict copy, nothing overwritten.
    let v3 = upload(&mut c, b"Plan v3 vom Mac").await;
    let conflict = op(&mut c, 13, &up(rev_of(&plan), v3)).await;
    assert!(
        matches!(conflict, RemoteResult::Conflict { .. }),
        "{conflict:?}"
    );
    assert_eq!(fs::read(dir.join("Projekte/plan.txt")).unwrap(), b"Plan v2");
    let copy_name = db::node_by_id(&env.db.pool, node_of(&conflict).0 as i64)
        .await
        .unwrap()
        .unwrap()
        .name;
    assert!(
        copy_name.starts_with("plan (Konflikt – Klaus' MacBook "),
        "{copy_name}"
    );
    assert_eq!(
        fs::read(dir.join("Projekte").join(&copy_name)).unwrap(),
        b"Plan v3 vom Mac"
    );
    // Outdated, but the file already has exactly this content: nothing to do.
    let same = op(&mut c, 14, &up(rev_of(&plan), v2)).await;
    assert_eq!(rev_of(&same), rev2);

    // Moves: only from where the client saw it; never into itself; never onto a taken name.
    let archiv = node_of(&op(&mut c, 20, &mkdir(root, "Archiv")).await);
    let mv =
        |node: NodeId, from_parent: NodeId, from: &str, parent: NodeId, to: &str| RemoteOp::Move {
            node,
            from_parent,
            from_name: name(from),
            parent,
            name: name(to),
        };
    assert_eq!(
        op(
            &mut c,
            21,
            &mv(plan_id, root, "plan.txt", archiv, "plan.txt")
        )
        .await,
        RemoteResult::Rejected(Reject::Moved)
    );
    assert_eq!(
        op(&mut c, 22, &mv(p, root, "Projekte", p, "x")).await,
        RemoteResult::Rejected(Reject::WouldCycle)
    );
    assert_eq!(
        op(
            &mut c,
            23,
            &mv(plan_id, p, "plan.txt", root, "plan kopie.txt")
        )
        .await,
        RemoteResult::Rejected(Reject::NameTaken)
    );
    assert!(matches!(
        op(
            &mut c,
            24,
            &mv(plan_id, p, "plan.txt", archiv, "plan 2026.txt")
        )
        .await,
        RemoteResult::Moved { .. }
    ));
    assert_eq!(
        fs::read(dir.join("Archiv/plan 2026.txt")).unwrap(),
        b"Plan v2"
    );

    // Deleting a file: only the revision and place the client knows.
    let del = |base: Rev, parent: NodeId, n: &str| RemoteOp::DeleteFile {
        node: plan_id,
        base_rev: base,
        parent,
        name: name(n),
    };
    assert_eq!(
        op(&mut c, 30, &del(rev_of(&plan), archiv, "plan 2026.txt")).await,
        RemoteResult::Rejected(Reject::RevMismatch)
    );
    assert_eq!(
        op(&mut c, 31, &del(rev2, p, "plan.txt")).await,
        RemoteResult::Rejected(Reject::Moved)
    );
    assert!(matches!(
        op(&mut c, 32, &del(rev2, archiv, "plan 2026.txt")).await,
        RemoteResult::Deleted { .. }
    ));
    assert!(!dir.join("Archiv/plan 2026.txt").exists());
    assert_eq!(
        op(&mut c, 33, &del(rev2, archiv, "plan 2026.txt")).await,
        RemoteResult::Rejected(Reject::NodeGone)
    );
    let trash = c.get(&format!("/api/roots/{root_id}/trash")).await;
    assert_eq!(
        trash.ok().as_array().unwrap().len(),
        1,
        "in the trash, not gone"
    );

    // Deleting a folder: only when empty.
    let rmdir = |node: NodeId, n: &str| RemoteOp::DeleteDir {
        node,
        parent: root,
        name: name(n),
    };
    assert_eq!(
        op(&mut c, 40, &rmdir(p, "Projekte")).await,
        RemoteResult::Rejected(Reject::NotEmpty)
    );
    assert!(matches!(
        op(&mut c, 41, &rmdir(archiv, "Archiv")).await,
        RemoteResult::Deleted { .. }
    ));

    // Someone else's nodes do not exist for this person.
    let (mut other, _, _, _) = signed_in(&env, "fremd").await;
    assert_eq!(
        op(&mut other, 1, &mkdir(p, "x")).await,
        RemoteResult::Rejected(Reject::ParentGone)
    );
    assert_eq!(
        op(&mut other, 2, &mv(p, root, "Projekte", root, "y")).await,
        RemoteResult::Rejected(Reject::NodeGone)
    );
    // Device names are part of the identity: the same id from another device is another operation.
    let r = other
        .post("/api/sync/ops", json!({"device": "", "op_id": 1, "op": serde_json::to_value(mkdir(root, "z")).unwrap()}))
        .await;
    assert_eq!(r.status, 400);

    // Everything agrees with a scan.
    let root_row = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        roots::scan(&env.state, &root_row).await.unwrap().changes(),
        0
    );
    env.finish().await;
}

#[tokio::test]
async fn aenderung_von_aussen_wird_nicht_ueberfahren() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "lena").await;
    write(&dir.join("notiz.txt"), b"alt");
    let root_row = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &root_row).await.unwrap();
    let n = node(&env, &root_row, "notiz.txt").await;
    let root = NodeId(root_node as u64);
    let rename = RemoteOp::Move {
        node: NodeId(n.id as u64),
        from_parent: root,
        from_name: name("notiz.txt"),
        parent: root,
        name: name("Notiz.txt"),
    };

    // Renamed over SMB, not scanned yet: not executed, try again …
    fs::rename(dir.join("notiz.txt"), dir.join("anders.txt")).unwrap();
    assert_eq!(op(&mut c, 1, &rename).await, RemoteResult::Transient);
    // … and the retry (same id, not recorded) decides with the state found meanwhile.
    assert_eq!(
        op(&mut c, 1, &rename).await,
        RemoteResult::Rejected(Reject::Moved)
    );
    assert!(dir.join("anders.txt").exists());
    env.finish().await;
}

async fn root_row(env: &Env, root_id: i64) -> db::RootRow {
    db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn nfd_name_verschieben_und_loeschen() {
    // A name written in NFD (macOS over SMB) stays NFD in the database; the feed gives clients
    // the NFC form. Preconditions compare both forms.
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "nfd").await;
    write(&dir.join("A\u{0308}pfel.txt"), b"apfel");
    write(&dir.join("A\u{0308}rger.txt"), b"aerger");
    roots::scan(&env.state, &root_row(&env, root_id).await)
        .await
        .unwrap();
    let rr = root_row(&env, root_id).await;
    let apfel = node(&env, &rr, "A\u{0308}pfel.txt").await;
    let aerger = node(&env, &rr, "A\u{0308}rger.txt").await;
    assert_eq!(apfel.name, "A\u{0308}pfel.txt", "stored as found on disk");
    let root = NodeId(root_node as u64);
    let moved = op(
        &mut c,
        1,
        &RemoteOp::Move {
            node: NodeId(apfel.id as u64),
            from_parent: root,
            from_name: name("\u{00C4}pfel.txt"),
            parent: root,
            name: name("Birne.txt"),
        },
    )
    .await;
    assert!(matches!(moved, RemoteResult::Moved { .. }), "{moved:?}");
    assert!(dir.join("Birne.txt").is_file());
    let deleted = op(
        &mut c,
        2,
        &RemoteOp::DeleteFile {
            node: NodeId(aerger.id as u64),
            base_rev: Rev(aerger.rev as u64),
            parent: root,
            name: name("\u{00C4}rger.txt"),
        },
    )
    .await;
    assert!(
        matches!(deleted, RemoteResult::Deleted { .. }),
        "{deleted:?}"
    );
    assert!(!dir.join("A\u{0308}rger.txt").exists());
    env.finish().await;
}

#[tokio::test]
async fn name_mit_leerzeichen_am_rand() {
    // Sync clients mirror names exactly: no trimming, control characters allowed (only what
    // the scanner accepts from disk, too).
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "rand").await;
    let root = NodeId(root_node as u64);
    let wichtig = op(
        &mut c,
        1,
        &RemoteOp::CreateDir {
            parent: root,
            name: name(" Wichtig"),
            source: LocalId(1),
            origin: Origin::New,
        },
    )
    .await;
    assert!(
        matches!(wichtig, RemoteResult::Created { .. }),
        "{wichtig:?}"
    );
    assert!(dir.join(" Wichtig").is_dir());
    let fc = upload(&mut c, b"Rechnung").await;
    let rechnung = op(
        &mut c,
        2,
        &RemoteOp::CreateFile {
            parent: root,
            name: name("Rechnung "),
            content: fc,
            source: LocalId(2),
            fp: FP,
            origin: Origin::New,
        },
    )
    .await;
    assert!(dir.join("Rechnung ").is_file(), "{rechnung:?}");
    let moved = op(
        &mut c,
        3,
        &RemoteOp::Move {
            node: node_of(&rechnung),
            from_parent: root,
            from_name: name("Rechnung "),
            parent: root,
            name: name("a\u{1}b"),
        },
    )
    .await;
    assert!(matches!(moved, RemoteResult::Moved { .. }), "{moved:?}");
    assert!(dir.join("a\u{1}b").is_file());
    // Reserved names stay refused for sync clients, too.
    let r = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 4, "op": {"CreateDir": {
                "parent": root_node, "name": ".DS_Store", "source": 3, "origin": "New"}}}),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
    // A name made over SMB can be moved by a sync client without being renamed.
    fs::create_dir(dir.join(" Archiv ")).unwrap();
    let rr = root_row(&env, root_id).await;
    roots::scan(&env.state, &rr).await.unwrap();
    let archiv = node(&env, &rr, " Archiv ").await;
    let moved = op(
        &mut c,
        5,
        &RemoteOp::Move {
            node: NodeId(archiv.id as u64),
            from_parent: root,
            from_name: name(" Archiv "),
            parent: node_of(&wichtig),
            name: name(" Archiv "),
        },
    )
    .await;
    assert!(matches!(moved, RemoteResult::Moved { .. }), "{moved:?}");
    assert!(dir.join(" Wichtig/ Archiv ").is_dir());
    // People still get trimmed names in the browser.
    let r = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({ "name": "  Neu  " }),
        )
        .await;
    assert_eq!(r.ok()["name"], "Neu");
    env.finish().await;
}

#[tokio::test]
async fn op_body_abweichung_409() {
    let env = env_or_skip!();
    let (mut c, _, root_node, dir) = signed_in(&env, "body").await;
    let root = NodeId(root_node as u64);
    let mkdir = |n: &str, source: u64| RemoteOp::CreateDir {
        parent: root,
        name: name(n),
        source: LocalId(source),
        origin: Origin::New,
    };
    let a = op(&mut c, 5, &mkdir("A", 1)).await;
    // The same ID for a different operation: refused, nothing executed.
    let r = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 5, "op": serde_json::to_value(mkdir("B", 2)).unwrap()}),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(r.body["reason"], "op_mismatch");
    assert!(!dir.join("B").exists());
    // The same operation again: the stored result. Only the client's own bookkeeping (here the
    // local ID) differs: still the same operation.
    assert_eq!(op(&mut c, 5, &mkdir("A", 1)).await, a);
    assert_eq!(op(&mut c, 5, &mkdir("A", 99)).await, a);
    // Lookup: bare result as before, or with the operation.
    let r = c.get(&format!("/api/sync/ops/{DEVICE_URL}/5")).await;
    assert_eq!(
        serde_json::from_value::<RemoteResult>(r.ok().clone()).unwrap(),
        a
    );
    for q in ["true", "1"] {
        let r = c
            .get(&format!("/api/sync/ops/{DEVICE_URL}/5?with_op={q}"))
            .await;
        assert_eq!(
            serde_json::from_value::<RemoteResult>(r.ok()["result"].clone()).unwrap(),
            a
        );
        assert_eq!(
            serde_json::from_value::<RemoteOp>(r.ok()["op"].clone()).unwrap(),
            mkdir("A", 1)
        );
    }
    // Rows stored before the server kept operations answer any operation as before.
    sqlx::query("UPDATE sync_ops SET op = NULL WHERE op_id = 5")
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(op(&mut c, 5, &mkdir("C", 3)).await, a);
    assert!(!dir.join("C").exists());
    let r = c
        .get(&format!("/api/sync/ops/{DEVICE_URL}/5?with_op=true"))
        .await;
    assert!(r.ok()["op"].is_null());
    env.finish().await;
}

#[tokio::test]
async fn op_mit_veraltetem_cursor_tag_409() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "tag").await;
    // An empty root has no cursor yet (0, no tag): one change first.
    write(&dir.join("a.txt"), b"a");
    roots::scan(&env.state, &root_row(&env, root_id).await)
        .await
        .unwrap();
    let feed = c
        .get(&format!("/api/sync/changes?root={root_id}&cursor=0"))
        .await;
    let cursor = feed.ok()["cursor"].as_i64().unwrap();
    let tag = feed.ok()["cursor_tag"].as_str().unwrap().to_owned();
    let mkdir = |n: &str| {
        serde_json::to_value(RemoteOp::CreateDir {
            parent: NodeId(root_node as u64),
            name: name(n),
            source: LocalId(1),
            origin: Origin::New,
        })
        .unwrap()
    };
    let ok = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 1, "op": mkdir("Gut"), "cursor": cursor, "check": tag}),
        )
        .await;
    assert!(ok.ok().get("Created").is_some(), "{}", ok.body);
    // The database "was restored": the entry at the client's cursor is no longer the same.
    sqlx::query("UPDATE journal SET at = at + interval '1 second' WHERE seq = $1")
        .bind(cursor)
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 2, "op": mkdir("Schlecht"), "cursor": cursor, "check": tag}),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(r.body["reason"], "cursor_invalid");
    // Checked before the stored results, too: a repeated operation is refused as well.
    let r = c
        .post(
            "/api/sync/ops",
            json!({"device": DEVICE, "op_id": 1, "op": mkdir("Gut"), "cursor": cursor, "check": tag}),
        )
        .await;
    assert_eq!(r.body["reason"], "cursor_invalid");
    env.finish().await;
}

#[tokio::test]
async fn op_abfrage_wartet_auf_laufende_op() {
    let env = env_or_skip!();
    let (c, _, _, _) = signed_in(&env, "warten").await;
    let user_id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE username = 'warten'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    // An operation of this device is running (holds the device's lock) …
    let lock = env.state.sync_lock(user_id, DEVICE);
    let guard = lock.lock().await;
    let mut asker = c.clone();
    let lookup = tokio::spawn(async move {
        asker
            .get(&format!("/api/sync/ops/{DEVICE_URL}/7"))
            .await
            .body
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(!lookup.is_finished(), "must wait, not answer 404");
    // … and finishes with a stored result.
    let result = RemoteResult::Rejected(Reject::NameTaken);
    sqlx::query("INSERT INTO sync_ops (user_id, device, op_id, result) VALUES ($1, $2, 7, $3)")
        .bind(user_id)
        .bind(DEVICE)
        .bind(serde_json::to_value(&result).unwrap())
        .execute(&env.db.pool)
        .await
        .unwrap();
    drop(guard);
    let body = lookup.await.unwrap();
    assert_eq!(
        serde_json::from_value::<RemoteResult>(body).unwrap(),
        result
    );
    env.finish().await;
}

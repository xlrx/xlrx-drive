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

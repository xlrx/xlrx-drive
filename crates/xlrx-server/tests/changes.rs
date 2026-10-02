//! Changes through the API: folders, rename and move, trash, restore, purge, and recovery after a
//! crash at every step. Runs twice: renaming on one file system, and copying as between the
//! Btrfs subvolumes of the NAS.

mod common;

use std::fs;
use std::path::Path;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::files::{db, ops, roots};

macro_rules! env_or_skip {
    ($e:expr) => {
        match $e.await {
            Some(e) => e,
            None => return,
        }
    };
}

async fn children(c: &mut Client, id: i64) -> Value {
    c.get(&format!("/api/nodes/{id}/children"))
        .await
        .ok()
        .clone()
}

fn id(v: &Value) -> i64 {
    v["id"].as_i64().unwrap()
}

/// Every file below `dir` with its content, for before/after comparisons.
fn snapshot(dir: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            let rel = p.strip_prefix(dir).unwrap().to_string_lossy().into_owned();
            if p.is_dir() {
                out.push((rel, "/".into()));
                stack.push(p);
            } else {
                out.push((rel, fs::read_to_string(&p).unwrap()));
            }
        }
    }
    out.sort();
    out
}

#[tokio::test]
async fn ordner_anlegen() {
    let env = env_or_skip!(Env::with_data());
    let (mut c, root_id, root_node, dir) = signed_in(&env, "anna").await;

    let r = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "  Projekte "}),
        )
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["name"], "Projekte");
    assert!(dir.join("Projekte").is_dir());
    let projekte = id(&r.body);

    // NFD from a Mac is stored as NFC.
    let r = c
        .post(
            &format!("/api/nodes/{projekte}/folders"),
            json!({"name": "A\u{308}nderungen"}),
        )
        .await;
    assert_eq!(r.ok()["name"], "Änderungen");
    assert!(dir.join("Projekte/Änderungen").is_dir());

    // Taken, ignoring case – in the database or only on disk (not scanned yet).
    let taken = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "projekte"}),
        )
        .await;
    assert_eq!(taken.status, 409);
    assert_eq!(taken.err(), "In diesem Ordner gibt es schon „projekte“.");
    fs::create_dir(dir.join("Extern")).unwrap();
    let r = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "EXTERN"}),
        )
        .await;
    assert_eq!(r.status, 409);

    for bad in [
        "",
        "  ",
        "a/b",
        ".DS_Store",
        "~$Bericht.docx",
        ".xlrx-srv-1",
        "Zeile\nzwei",
        "..",
    ] {
        let r = c
            .post(
                &format!("/api/nodes/{root_node}/folders"),
                json!({ "name": bad }),
            )
            .await;
        assert_eq!(r.status, 400, "{bad:?}: {}", r.body);
    }
    // Not into a file.
    write(&dir.join("datei.txt"), b"x");
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &root).await.unwrap();
    let file = id(find(&children(&mut c, root_node).await, "datei.txt"));
    let r = c
        .post(&format!("/api/nodes/{file}/folders"), json!({"name": "x"}))
        .await;
    assert_eq!(r.status, 400);

    // The scan agrees with what the API did.
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.changes(), 0, "{r:?}");
    assert_eq!(journal_ops(&env, projekte).await, ["create"]);
    env.finish().await;
}

#[tokio::test]
async fn umbenennen_und_verschieben() {
    let env = env_or_skip!(Env::with_data());
    let (mut c, root_id, root_node, dir) = signed_in(&env, "ben").await;
    write(&dir.join("Projekte/plan.txt"), b"Plan");
    write(&dir.join("Projekte/Unter/tief.txt"), b"tief");
    write(&dir.join("Archiv/plan.txt"), b"anderer Plan");
    write(&dir.join("notiz.md"), b"Notiz");
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &root).await.unwrap();
    let top = children(&mut c, root_node).await;
    let (projekte, archiv, notiz) = (
        id(find(&top, "Projekte")),
        id(find(&top, "Archiv")),
        find(&top, "notiz.md").clone(),
    );

    // Rename.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{}", id(&notiz)),
            Some(json!({"name": "Notiz 2026.md"})),
        )
        .await;
    assert_eq!(r.ok()["name"], "Notiz 2026.md");
    assert_eq!(
        fs::read_to_string(dir.join("Notiz 2026.md")).unwrap(),
        "Notiz"
    );
    assert!(!dir.join("notiz.md").exists());
    // Only the case.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{}", id(&notiz)),
            Some(json!({"name": "NOTIZ 2026.md"})),
        )
        .await;
    assert_eq!(r.ok()["name"], "NOTIZ 2026.md");
    assert!(dir.join("NOTIZ 2026.md").exists());

    // Stale view: the node changed since it was listed.
    let stale = c
        .send(
            "PATCH",
            &format!("/api/nodes/{}", id(&notiz)),
            Some(json!({"name": "x.md", "if_seq": notiz["seq"]})),
        )
        .await;
    assert_eq!(stale.status, 409);

    // Move: the target has a file with that name – nothing is replaced.
    let plan = id(find(&children(&mut c, projekte).await, "plan.txt"));
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{plan}"),
            Some(json!({"parent_id": archiv})),
        )
        .await;
    assert_eq!(r.status, 409, "{}", r.body);
    assert_eq!(
        fs::read_to_string(dir.join("Archiv/plan.txt")).unwrap(),
        "anderer Plan"
    );
    assert_eq!(
        fs::read_to_string(dir.join("Projekte/plan.txt")).unwrap(),
        "Plan"
    );
    // Move with a new name in one step.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{plan}"),
            Some(json!({"parent_id": archiv, "name": "plan alt.txt"})),
        )
        .await;
    assert_eq!(r.ok()["parent_id"], archiv);
    assert_eq!(
        fs::read_to_string(dir.join("Archiv/plan alt.txt")).unwrap(),
        "Plan"
    );

    // A folder never moves into itself or below itself.
    let unter = id(find(&children(&mut c, projekte).await, "Unter"));
    for target in [projekte, unter] {
        let r = c
            .send(
                "PATCH",
                &format!("/api/nodes/{projekte}"),
                Some(json!({"parent_id": target})),
            )
            .await;
        assert_eq!(r.status, 400, "{}", r.body);
    }
    // Move a folder with content: one node moves, the content comes along.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{projekte}"),
            Some(json!({"parent_id": archiv})),
        )
        .await;
    assert_eq!(r.ok()["parent_id"], archiv);
    assert_eq!(
        fs::read_to_string(dir.join("Archiv/Projekte/Unter/tief.txt")).unwrap(),
        "tief"
    );
    // The root itself stays.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{root_node}"),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(r.status, 400);

    // The scan agrees, and renamed files are not read again (fingerprint kept current).
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    roots::scan(&env.state, &root).await.unwrap();
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{plan}"),
            Some(json!({"name": "plan 2025.txt"})),
        )
        .await;
    r.ok();
    let report = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(
        (report.changes(), report.hashed_bytes),
        (0, 0),
        "{report:?}"
    );
    assert_eq!(journal_ops(&env, plan).await, ["create", "move", "move"]);
    env.finish().await;
}

#[tokio::test]
async fn aenderung_ausserhalb_wird_erkannt_statt_ueberschrieben() {
    let env = env_or_skip!(Env::with_data());
    let (mut c, root_id, root_node, dir) = signed_in(&env, "carla").await;
    write(&dir.join("a.txt"), b"a");
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &root).await.unwrap();
    let a = id(find(&children(&mut c, root_node).await, "a.txt"));

    // Renamed over SMB, the server has not noticed yet.
    fs::rename(dir.join("a.txt"), dir.join("b.txt")).unwrap();
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{a}"),
            Some(json!({"name": "c.txt"})),
        )
        .await;
    assert_eq!(r.status, 409);
    assert!(r.err().contains("außerhalb von xlrx"), "{}", r.err());
    // The refusal brought the view up to date: same node, new name.
    assert_eq!(
        c.get(&format!("/api/nodes/{a}")).await.ok()["name"],
        "b.txt"
    );
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{a}"),
            Some(json!({"name": "c.txt"})),
        )
        .await;
    r.ok();
    assert!(dir.join("c.txt").exists());

    // Replaced by another file under the same name: also refused, never trashed blindly.
    atomic_save(&dir.join("c.txt"), b"neu");
    let r = c.send("DELETE", &format!("/api/nodes/{a}"), None).await;
    assert_eq!(r.status, 409);
    assert_eq!(fs::read_to_string(dir.join("c.txt")).unwrap(), "neu");
    env.finish().await;
}

async fn papierkorb(env: Env) {
    let (mut c, root_id, root_node, dir) = signed_in(&env, "dora").await;
    write(&dir.join("Projekte/plan.txt"), b"Plan");
    write(&dir.join("Projekte/Unter/tief.txt"), b"tief");
    write(&dir.join("Projekte/Unter/leer/.keep"), b"");
    write(&dir.join("notiz.md"), b"Notiz");
    let root = db::root_by_id(&env.db.pool, root_id)
        .await
        .unwrap()
        .unwrap();
    roots::scan(&env.state, &root).await.unwrap();
    let before = snapshot(&dir);
    let top = children(&mut c, root_node).await;
    let projekte = find(&top, "Projekte").clone();
    let ids_before: Vec<(String, i64)> = tree(&env, &root)
        .await
        .into_iter()
        .map(|(p, n)| (p, n.id))
        .collect();

    // Delete a folder: gone from disk and view, in the trash as one item.
    let r = c
        .send(
            "DELETE",
            &format!("/api/nodes/{}?if_seq={}", id(&projekte), projekte["seq"]),
            None,
        )
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert!(!dir.join("Projekte").exists());
    assert_eq!(names(&env, &root).await, ["notiz.md"]);
    let trash = c.get(&format!("/api/roots/{root_id}/trash")).await;
    let items = trash.ok().as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["name"], "Projekte");
    assert_eq!(items[0]["from"], "Meine Ablage");
    let plan = ids_before
        .iter()
        .find(|(p, _)| p == "Projekte/plan.txt")
        .unwrap()
        .1;
    assert_eq!(journal_ops(&env, plan).await, ["create", "delete"]);
    assert!(
        fs::read_dir(env.data_dir().join("xlrx-state/store/staging"))
            .unwrap()
            .next()
            .is_none()
    );

    // Delete a file too, then restore the folder: same nodes, same content, same place.
    let notiz = id(find(&children(&mut c, root_node).await, "notiz.md"));
    assert_eq!(
        c.send("DELETE", &format!("/api/nodes/{notiz}"), None)
            .await
            .status,
        204
    );
    let r = c
        .post(&format!("/api/trash/{}/restore", id(&projekte)), json!({}))
        .await;
    assert_eq!(r.ok()["name"], "Projekte");
    let ids_after: Vec<(String, i64)> = tree(&env, &root)
        .await
        .into_iter()
        .map(|(p, n)| (p, n.id))
        .collect();
    let without_notiz: Vec<_> = ids_before
        .iter()
        .filter(|(p, _)| p != "notiz.md")
        .cloned()
        .collect();
    assert_eq!(ids_after, without_notiz);
    assert_eq!(
        journal_ops(&env, plan).await,
        ["create", "delete", "restore"]
    );
    // The restored items carry their identity on disk (copies have new inodes): changing them
    // right away works without a scan in between.
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{plan}"),
            Some(json!({"name": "plan2.txt"})),
        )
        .await;
    r.ok();
    let r = c
        .send(
            "PATCH",
            &format!("/api/nodes/{plan}"),
            Some(json!({"name": "plan.txt"})),
        )
        .await;
    r.ok();
    let r = c
        .post(&format!("/api/trash/{notiz}/restore"), json!({}))
        .await;
    r.ok();
    assert_eq!(snapshot(&dir), before);
    assert!(
        c.get(&format!("/api/roots/{root_id}/trash"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
    // Nothing left in the trash store.
    assert!(
        fs::read_dir(env.data_dir().join("xlrx-state/store/trash"))
            .unwrap()
            .next()
            .is_none()
    );
    // The scan agrees: same nodes, no changes.
    let report = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(report.changes(), 0, "{report:?}");

    // Restore when the name is taken meanwhile, and when the folder is gone.
    let notiz = id(find(&children(&mut c, root_node).await, "notiz.md"));
    assert_eq!(
        c.send("DELETE", &format!("/api/nodes/{notiz}"), None)
            .await
            .status,
        204
    );
    write(&dir.join("notiz.md"), b"neue Notiz");
    let unter = id(find(&children(&mut c, id(&projekte)).await, "Unter"));
    let tief = id(find(&children(&mut c, unter).await, "tief.txt"));
    assert_eq!(
        c.send("DELETE", &format!("/api/nodes/{tief}"), None)
            .await
            .status,
        204
    );
    assert_eq!(
        c.send("DELETE", &format!("/api/nodes/{}", id(&projekte)), None)
            .await
            .status,
        204
    );
    let r = c
        .post(&format!("/api/trash/{notiz}/restore"), json!({}))
        .await;
    assert_eq!(r.ok()["name"], "notiz (wiederhergestellt).md");
    assert_eq!(
        fs::read_to_string(dir.join("notiz.md")).unwrap(),
        "neue Notiz"
    );
    assert_eq!(
        fs::read_to_string(dir.join("notiz (wiederhergestellt).md")).unwrap(),
        "Notiz"
    );
    let r = c
        .post(&format!("/api/trash/{tief}/restore"), json!({}))
        .await;
    assert_eq!(r.ok()["parent_id"], root_node, "folder gone: to the top");
    assert_eq!(fs::read_to_string(dir.join("tief.txt")).unwrap(), "tief");

    // Delete for good.
    assert_eq!(
        c.send("DELETE", &format!("/api/trash/{}", id(&projekte)), None)
            .await
            .status,
        204
    );
    assert!(
        c.get(&format!("/api/roots/{root_id}/trash"))
            .await
            .ok()
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        c.post(&format!("/api/trash/{}/restore", id(&projekte)), json!({}))
            .await
            .status,
        404
    );
    assert!(
        fs::read_dir(env.data_dir().join("xlrx-state/store/trash"))
            .unwrap()
            .next()
            .is_none()
    );

    // Nobody else gets anywhere near it.
    let (mut other, other_root, _, _) = signed_in(&env, "emil").await;
    let r = other
        .send("DELETE", &format!("/api/nodes/{notiz}"), None)
        .await;
    assert_eq!(r.status, 404);
    let r = other
        .send(
            "PATCH",
            &format!("/api/nodes/{notiz}"),
            Some(json!({"name": "x"})),
        )
        .await;
    assert_eq!(r.status, 404);
    let r = other
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "x"}),
        )
        .await;
    assert_eq!(r.status, 404);
    assert_eq!(
        other
            .get(&format!("/api/roots/{root_id}/trash"))
            .await
            .status,
        404
    );
    assert_ne!(other_root, root_id);
    env.finish().await;
}

#[tokio::test]
async fn papierkorb_im_selben_dateisystem() {
    papierkorb(env_or_skip!(Env::with_data())).await;
}

#[tokio::test]
async fn papierkorb_zwischen_subvolumes() {
    papierkorb(env_or_skip!(Env::with_data_copying())).await;
}

/// State after a crash: an intent in `pending_ops` and the disk somewhere in between.
async fn intent(env: &Env, payload: Value) {
    let kind = payload["op"].as_str().unwrap().to_owned();
    sqlx::query("INSERT INTO pending_ops (kind, payload) VALUES ($1, $2)")
        .bind(kind)
        .bind(payload)
        .execute(&env.db.pool)
        .await
        .unwrap();
}

async fn pending(env: &Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pending_ops")
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

fn copy_dir(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).unwrap();
    for e in fs::read_dir(src).unwrap() {
        let e = e.unwrap();
        if e.path().is_dir() {
            copy_dir(&e.path(), &dst.join(e.file_name()));
        } else {
            fs::copy(e.path(), dst.join(e.file_name())).unwrap();
        }
    }
}

#[tokio::test]
async fn absturz_beim_loeschen() {
    let env = env_or_skip!(Env::with_data());
    let (root, dir) = home_of(&env, "fritz").await;
    let user = root.owner_user_id.unwrap();
    for name in ["A", "B", "C"] {
        write(&dir.join(format!("{name}/datei.txt")), name.as_bytes());
    }
    roots::scan(&env.state, &root).await.unwrap();
    let state = env.data_dir().join("xlrx-state");
    fs::create_dir_all(state.join("store/trash")).unwrap();
    let rel = |n: &str| format!("homes/fritz/Drive/{n}");
    let (a, b, c) = (
        node(&env, &root, "A").await,
        node(&env, &root, "B").await,
        node(&env, &root, "C").await,
    );

    // A: copy started, not complete – the original stays, the partial copy too (never removed
    // blindly: it could be the only copy).
    fs::create_dir_all(state.join("store/trash/a/A")).unwrap();
    intent(
        &env,
        json!({"op": "trash", "root_id": root.id, "node_id": a.id, "actor": user,
        "src": rel("A"), "dst": "store/trash/a/A", "copied": false}),
    )
    .await;
    // B: copy complete and verified, source partly removed.
    copy_dir(&dir.join("B"), &state.join("store/trash/b/B"));
    fs::remove_file(dir.join("B/datei.txt")).unwrap();
    intent(
        &env,
        json!({"op": "trash", "root_id": root.id, "node_id": b.id, "actor": user,
        "src": rel("B"), "dst": "store/trash/b/B", "copied": true}),
    )
    .await;
    // C: renamed into the trash (same file system), database not updated.
    fs::create_dir_all(state.join("store/trash/c")).unwrap();
    fs::rename(dir.join("C"), state.join("store/trash/c/C")).unwrap();
    intent(
        &env,
        json!({"op": "trash", "root_id": root.id, "node_id": c.id, "actor": user,
        "src": rel("C"), "dst": "store/trash/c/C", "copied": false}),
    )
    .await;

    ops::recover(&env.state, None).await.unwrap();
    assert_eq!(pending(&env).await, 0);
    assert_eq!(names(&env, &root).await, ["A", "A/datei.txt"]);
    assert_eq!(fs::read_to_string(dir.join("A/datei.txt")).unwrap(), "A");
    assert!(state.join("store/trash/a/A").exists(), "partial copy kept");
    assert!(!dir.join("B").exists());
    assert_eq!(
        fs::read_to_string(state.join("store/trash/b/B/datei.txt")).unwrap(),
        "B"
    );
    let trash = ops::trash_list(&env.state, user, root.id).await.unwrap();
    let mut trashed: Vec<&str> = trash.iter().map(|t| t.name.as_str()).collect();
    trashed.sort();
    assert_eq!(trashed, ["B", "C"]);
    // And both come back.
    for n in [&b, &c] {
        ops::restore(&env.state, user, n.id).await.unwrap();
    }
    assert_eq!(
        names(&env, &root).await,
        ["A", "A/datei.txt", "B", "B/datei.txt", "C", "C/datei.txt"]
    );
    assert_eq!(fs::read_to_string(dir.join("B/datei.txt")).unwrap(), "B");
    assert_eq!(roots::scan(&env.state, &root).await.unwrap().changes(), 0);
    env.finish().await;
}

#[tokio::test]
async fn absturz_beim_wiederherstellen() {
    let env = env_or_skip!(Env::with_data_copying());
    let (root, dir) = home_of(&env, "greta").await;
    let user = root.owner_user_id.unwrap();
    for name in ["A", "B", "C"] {
        write(&dir.join(format!("{name}/datei.txt")), name.as_bytes());
    }
    roots::scan(&env.state, &root).await.unwrap();
    let ids: Vec<i64> = {
        let mut v = Vec::new();
        for name in ["A", "B", "C"] {
            let n = node(&env, &root, name).await;
            ops::trash(&env.state, user, n.id, None).await.unwrap();
            v.push(n.id);
        }
        v
    };
    let state = env.data_dir().join("xlrx-state");
    let trash_of = |id: i64| {
        let c = fs::read_dir(state.join("store/trash"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .find(|n| n.starts_with(&format!("{id}-")))
            .unwrap();
        format!("store/trash/{c}")
    };
    let (ta, tb, tc) = (trash_of(ids[0]), trash_of(ids[1]), trash_of(ids[2]));
    let rel = |n: &str| format!("homes/greta/Drive/{n}");
    let root_node = db::root_node(&env.db.pool, root.id)
        .await
        .unwrap()
        .unwrap()
        .id;

    // A: staged copy in progress – removed, the item stays in the trash.
    copy_dir(&state.join(format!("{ta}/A")), &dir.join(".xlrx-srv-a"));
    intent(
        &env,
        json!({"op": "restore", "root_id": root.id, "node_id": ids[0], "actor": user,
        "src": format!("{ta}/A"), "dst": rel("A"), "stage": rel(".xlrx-srv-a"),
        "parent_id": root_node, "name": "A", "copied": false}),
    )
    .await;
    // B: copied and renamed into place, trash copy not removed yet.
    copy_dir(&state.join(format!("{tb}/B")), &dir.join("B"));
    intent(
        &env,
        json!({"op": "restore", "root_id": root.id, "node_id": ids[1], "actor": user,
        "src": format!("{tb}/B"), "dst": rel("B"), "stage": rel(".xlrx-srv-b"),
        "parent_id": root_node, "name": "B", "copied": true}),
    )
    .await;
    // C: renamed out of the trash, database not updated.
    fs::rename(state.join(format!("{tc}/C")), dir.join("C")).unwrap();
    intent(
        &env,
        json!({"op": "restore", "root_id": root.id, "node_id": ids[2], "actor": user,
        "src": format!("{tc}/C"), "dst": rel("C"), "stage": rel(".xlrx-srv-c"),
        "parent_id": root_node, "name": "C", "copied": false}),
    )
    .await;

    ops::recover(&env.state, None).await.unwrap();
    assert_eq!(pending(&env).await, 0);
    assert!(!dir.join(".xlrx-srv-a").exists());
    assert_eq!(
        names(&env, &root).await,
        ["B", "B/datei.txt", "C", "C/datei.txt"]
    );
    assert!(!state.join(&tb).exists() && !state.join(&tc).exists());
    let trash = ops::trash_list(&env.state, user, root.id).await.unwrap();
    assert_eq!(trash.iter().map(|t| t.id).collect::<Vec<_>>(), [ids[0]]);
    // Restored nodes carry their new identity on disk: the scan finds no changes.
    let report = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(report.changes(), 0, "{report:?}");
    assert_eq!(node(&env, &root, "B").await.id, ids[1]);
    assert_eq!(
        node(&env, &root, "C/datei.txt").await.parent_id,
        Some(ids[2])
    );
    env.finish().await;
}

#[tokio::test]
async fn aufraeumen_nach_ablauf() {
    let env = env_or_skip!(Env::with_data());
    let (root, dir) = home_of(&env, "hugo").await;
    let user = root.owner_user_id.unwrap();
    write(&dir.join("alt.txt"), b"alt");
    write(&dir.join("neu.txt"), b"neu");
    roots::scan(&env.state, &root).await.unwrap();
    let (alt, neu) = (
        node(&env, &root, "alt.txt").await,
        node(&env, &root, "neu.txt").await,
    );
    ops::trash(&env.state, user, alt.id, None).await.unwrap();
    ops::trash(&env.state, user, neu.id, None).await.unwrap();
    sqlx::query("UPDATE nodes SET deleted_at = now() - interval '31 days' WHERE id = $1")
        .bind(alt.id)
        .execute(&env.db.pool)
        .await
        .unwrap();
    // A leftover container nobody refers to: kept while young, removed when old.
    let state = env.data_dir().join("xlrx-state");
    let orphan = state.join("store/trash/999-verwaist");
    fs::create_dir_all(orphan.join("x")).unwrap();
    ops::housekeeping(&env.state).await.unwrap();
    assert!(orphan.exists());
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(31 * 86_400);
    fs::File::open(&orphan).unwrap().set_modified(old).unwrap();
    ops::housekeeping(&env.state).await.unwrap();
    assert!(!orphan.exists());
    let trash = ops::trash_list(&env.state, user, root.id).await.unwrap();
    assert_eq!(
        trash.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["neu.txt"]
    );
    let left: Vec<_> = fs::read_dir(state.join("store/trash")).unwrap().collect();
    assert_eq!(left.len(), 1);
    env.finish().await;
}

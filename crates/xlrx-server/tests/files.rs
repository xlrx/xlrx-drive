//! Roots, reconciliation scan, browsing and downloading, against a real PostgreSQL and a
//! temporary data directory.

mod common;

use std::fs;
use std::path::Path;

use common::files::*;
use common::*;
use xlrx_server::files::db;
use xlrx_server::files::roots;

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

#[tokio::test]
async fn abgleich_erkennt_aenderungen_ausserhalb_von_xlrx() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "anna").await;

    write(&dir.join("Projekte/plan.txt"), b"Plan v1");
    write(&dir.join("notiz.md"), b"# Notiz");
    // Never become nodes.
    write(&dir.join(".DS_Store"), b"x");
    write(&dir.join("~$Bericht.docx"), b"x");
    write(&dir.join("@eaDir/notiz.md/SYNOFILE_THUMB_M.jpg"), b"x");
    write(&dir.join(".xlrx-srv-17"), b"x");

    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (3, 0, 0, 0));
    assert_eq!(
        names(&env, &root).await,
        ["Projekte", "Projekte/plan.txt", "notiz.md"]
    );
    let plan = node(&env, &root, "Projekte/plan.txt").await;
    assert_eq!(plan.size, Some(7));
    assert_eq!(plan.content_hash.as_ref().map(Vec::len), Some(32));
    assert!(
        db::root_by_id(&env.db.pool, root.id)
            .await
            .unwrap()
            .unwrap()
            .scanned_at
            .is_some()
    );

    // Nothing changed: nothing to do.
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.changes(), 0, "{r:?}");

    // Rename and move: same node, new location.
    let notiz = node(&env, &root, "notiz.md").await;
    fs::rename(dir.join("notiz.md"), dir.join("Projekte/Notiz neu.md")).unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (0, 0, 1, 0));
    let moved = node(&env, &root, "Projekte/Notiz neu.md").await;
    assert_eq!(moved.id, notiz.id);
    assert_eq!(moved.rev, notiz.rev, "Inhalt unverändert");
    assert_eq!(moved.content_hash, notiz.content_hash);
    assert_eq!(journal_ops(&env, notiz.id).await, ["create", "move"]);

    // Atomic save (new inode at the same path): same node, new content and revision.
    atomic_save(&dir.join("Projekte/plan.txt"), b"Plan v2, laenger");
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (0, 1, 0, 0));
    let saved = node(&env, &root, "Projekte/plan.txt").await;
    assert_eq!(saved.id, plan.id);
    assert_eq!(saved.size, Some(16));
    assert_ne!(saved.content_hash, plan.content_hash);
    assert!(saved.rev > plan.rev);
    assert_eq!(journal_ops(&env, plan.id).await, ["create", "update"]);

    // Atomic save with identical content: no visible change.
    atomic_save(&dir.join("Projekte/plan.txt"), b"Plan v2, laenger");
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.changes(), 0, "{r:?}");
    assert_eq!(node(&env, &root, "Projekte/plan.txt").await.rev, saved.rev);

    // Rename plus a new file at the old path in one go: the renamed file keeps its node, even
    // though the new file is listed first.
    write(&dir.join("a.txt"), b"alt");
    roots::scan(&env.state, &root).await.unwrap();
    let a = node(&env, &root, "a.txt").await;
    fs::rename(dir.join("a.txt"), dir.join("b.txt")).unwrap();
    write(&dir.join("a.txt"), b"neu");
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (1, 0, 1, 0));
    assert_eq!(node(&env, &root, "b.txt").await.id, a.id);
    assert_ne!(node(&env, &root, "a.txt").await.id, a.id);

    // Move a whole folder: the folder moves, its children keep their relative place.
    fs::create_dir(dir.join("Archiv")).unwrap();
    fs::rename(dir.join("Projekte"), dir.join("Archiv/Projekte 2025")).unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (1, 0, 1, 0));
    assert_eq!(
        node(&env, &root, "Archiv/Projekte 2025/plan.txt").await.id,
        plan.id
    );

    // Delete a folder with content: every node is marked deleted.
    fs::remove_dir_all(dir.join("Archiv")).unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.updated, r.moved, r.deleted), (0, 0, 0, 4));
    assert_eq!(names(&env, &root).await, ["a.txt", "b.txt"]);
    let gone = db::node_by_id(&env.db.pool, plan.id)
        .await
        .unwrap()
        .unwrap();
    assert!(gone.deleted_at.is_some());
    // Moving the folder was not a move of the files inside it.
    assert_eq!(
        journal_ops(&env, plan.id).await,
        ["create", "update", "delete"]
    );

    // The journal is gapless in commit order and every change raised the node's `seq`.
    let seqs: Vec<i64> = sqlx::query_scalar("SELECT seq FROM journal ORDER BY seq")
        .fetch_all(&env.db.pool)
        .await
        .unwrap();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    let max_node_seq: i64 = sqlx::query_scalar("SELECT max(seq) FROM nodes")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(Some(&max_node_seq), seqs.last());
    env.finish().await;
}

#[tokio::test]
async fn unveraenderte_dateien_werden_nicht_erneut_gelesen() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "ben").await;
    write(&dir.join("gross.bin"), &vec![7u8; 300_000]);
    write(&dir.join("klein.txt"), b"hallo");
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.hashed_bytes, 300_005);

    // Just written: too close to hashing to trust the timestamp ("racy"), so it is read again.
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.changes(), r.hashed_bytes), (0, 300_005));

    // Once the timestamps are old enough, unchanged files are no longer read.
    tokio::time::sleep(std::time::Duration::from_millis(2100)).await;
    roots::scan(&env.state, &root).await.unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.changes(), r.hashed_bytes), (0, 0));

    // A rename (changes ctime) does not force a re-read either.
    fs::rename(dir.join("gross.bin"), dir.join("umbenannt.bin")).unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.moved, r.hashed_bytes), (1, 0));

    // Same size, mtime restored, content different: ctime gives it away.
    let path = dir.join("klein.txt");
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let before = node(&env, &root, "klein.txt").await;
    fs::write(&path, b"HALLO").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(mtime)
        .unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.updated, 1);
    assert_ne!(
        node(&env, &root, "klein.txt").await.content_hash,
        before.content_hash
    );
    env.finish().await;
}

#[tokio::test]
async fn wiederverwendete_inode_erbt_nichts() {
    use std::os::unix::fs::MetadataExt;
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "hanna").await;
    write(&dir.join("vertraulich.txt"), b"alt und geteilt");
    roots::scan(&env.state, &root).await.unwrap();
    let old = node(&env, &root, "vertraulich.txt").await;

    // ext4 hands a freed inode to the next new file. Simulated: the known node now carries the
    // inode of an unrelated new file elsewhere.
    fs::remove_file(dir.join("vertraulich.txt")).unwrap();
    write(&dir.join("Einkauf.txt"), b"Milch");
    let meta = fs::metadata(dir.join("Einkauf.txt")).unwrap();
    sqlx::query("UPDATE nodes SET fs_dev = $2, fs_ino = $3 WHERE id = $1")
        .bind(old.id)
        .bind(meta.dev() as i64)
        .bind(meta.ino() as i64)
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!((r.created, r.moved, r.deleted), (1, 0, 1));
    assert_ne!(node(&env, &root, "Einkauf.txt").await.id, old.id);
    env.finish().await;
}

#[tokio::test]
async fn neue_inode_am_selben_ort_wird_immer_gelesen() {
    use std::os::unix::fs::MetadataExt;
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "ida").await;
    let path = dir.join("bericht.txt");
    write(&path, b"Fassung A");
    roots::scan(&env.state, &root).await.unwrap();
    let before = node(&env, &root, "bericht.txt").await;

    // Replaced by a different file whose size and timestamps happen to match the stored ones.
    atomic_save(&path, b"Fassung B");
    let m = fs::metadata(&path).unwrap();
    sqlx::query("UPDATE nodes SET fs_size = $2, fs_mtime_ns = $3, fs_ctime_ns = $4 WHERE id = $1")
        .bind(before.id)
        .bind(m.size() as i64)
        .bind(m.mtime() * 1_000_000_000 + m.mtime_nsec())
        .bind(m.ctime() * 1_000_000_000 + m.ctime_nsec())
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.updated, 1);
    assert_ne!(
        node(&env, &root, "bericht.txt").await.content_hash,
        before.content_hash
    );
    env.finish().await;
}

#[tokio::test]
async fn leere_oder_fehlende_ablage_loescht_nichts() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "carla").await;
    for i in 0..25 {
        write(&dir.join(format!("Datei {i}.txt")), b"x");
    }
    assert_eq!(roots::scan(&env.state, &root).await.unwrap().created, 25);

    // Share not mounted: the directory is there but empty.
    let away = env.data_dir().join("weg");
    fs::rename(&dir, &away).unwrap();
    fs::create_dir(&dir).unwrap();
    let e = roots::scan(&env.state, &root).await.unwrap_err();
    assert!(format!("{e:?}").contains("leer"), "{e:?}");
    assert_eq!(names(&env, &root).await.len(), 25);

    // Directory missing altogether.
    fs::remove_dir(&dir).unwrap();
    assert!(roots::scan(&env.state, &root).await.is_err());
    assert_eq!(names(&env, &root).await.len(), 25);

    // Back again: everything as before, nothing created or deleted.
    fs::rename(&away, &dir).unwrap();
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(r.changes(), 0, "{r:?}");

    // A small root may really be emptied.
    let (small, small_dir) = home_of(&env, "dora").await;
    write(&small_dir.join("eine.txt"), b"x");
    roots::scan(&env.state, &small).await.unwrap();
    fs::remove_file(small_dir.join("eine.txt")).unwrap();
    assert_eq!(roots::scan(&env.state, &small).await.unwrap().deleted, 1);
    env.finish().await;
}

#[tokio::test]
async fn ungueltige_namen_und_symlinks_werden_uebersprungen() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "emil").await;
    write(&dir.join("ok.txt"), b"x");
    write(&dir.join("ziel/datei.txt"), b"x");
    std::os::unix::fs::symlink(dir.join("ziel"), dir.join("verweis")).unwrap();
    std::os::unix::fs::symlink("/etc/passwd", dir.join("passwd")).unwrap();
    // Not valid UTF-8 (possible over SMB with old clients).
    let raw = <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(b"kaputt-\xff.txt");
    write(&dir.join(raw), b"x");
    let r = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(
        names(&env, &root).await,
        ["ok.txt", "ziel", "ziel/datei.txt"]
    );
    assert_eq!(r.skipped.len(), 1, "{:?}", r.skipped);
    env.finish().await;
}

#[tokio::test]
async fn durchsuchen_und_herunterladen() {
    let env = env_or_skip!();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "frida").await;
    write(
        &dir.join("Fotos/strand.jpg"),
        b"\xff\xd8\xff\xe0 kein echtes JPEG",
    );
    write(&dir.join("Liesmich.txt"), b"0123456789");
    write(&dir.join("seite.html"), b"<script>alert(1)</script>");
    write(&dir.join("Bericht \"final\" ä.pdf"), b"%PDF-1.4");
    c.post(&format!("/api/roots/{root_id}/scan"), serde_json::json!({}))
        .await
        .ok();

    let list = c.get(&format!("/api/nodes/{root_node}/children")).await;
    let names: Vec<&str> = list
        .ok()
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    // Folders first, then files by name.
    assert_eq!(
        names,
        [
            "Fotos",
            "Bericht \"final\" ä.pdf",
            "Liesmich.txt",
            "seite.html"
        ]
    );
    let txt = find(&list.body, "Liesmich.txt");
    assert_eq!(txt["kind"], "file");
    assert_eq!(txt["size"], 10);
    assert_eq!(txt["mime"], "text/plain");
    let txt_id = txt["id"].as_i64().unwrap();

    let fotos = find(&list.body, "Fotos")["id"].as_i64().unwrap();
    let sub = c.get(&format!("/api/nodes/{fotos}/children")).await;
    let jpg = find(sub.ok(), "strand.jpg")["id"].as_i64().unwrap();
    let detail = c.get(&format!("/api/nodes/{jpg}")).await;
    let crumbs: Vec<&str> = detail.ok()["path"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(crumbs, ["Meine Ablage", "Fotos", "strand.jpg"]);
    assert_eq!(detail.body["root_id"], root_id);

    // Download: always as an attachment with a neutral type.
    let d = c
        .get_raw(&format!("/api/nodes/{txt_id}/content"), &[])
        .await;
    assert_eq!(d.status, 200);
    assert_eq!(d.bytes, b"0123456789");
    assert_eq!(d.header("content-type"), "application/octet-stream");
    assert!(d.header("content-disposition").starts_with("attachment;"));
    assert!(d.header("content-security-policy").starts_with("sandbox"));
    assert_eq!(d.header("x-content-type-options"), "nosniff");
    assert!(!d.header("etag").is_empty());

    // Resumable: range requests.
    let part = c
        .get_raw(
            &format!("/api/nodes/{txt_id}/content"),
            &[("range", "bytes=2-5")],
        )
        .await;
    assert_eq!(part.status, 206);
    assert_eq!(part.bytes, b"2345");
    assert_eq!(part.header("content-range"), "bytes 2-5/10");

    // Preview: harmless types inline, HTML never.
    let v = c
        .get_raw(&format!("/api/nodes/{txt_id}/content?inline=true"), &[])
        .await;
    assert!(v.header("content-type").starts_with("text/plain"));
    assert!(v.header("content-disposition").starts_with("inline;"));
    let html = find(&list.body, "seite.html")["id"].as_i64().unwrap();
    let h = c
        .get_raw(&format!("/api/nodes/{html}/content?inline=true"), &[])
        .await;
    assert_eq!(h.header("content-type"), "application/octet-stream");
    assert!(h.header("content-disposition").starts_with("attachment;"));
    let pdf = find(&list.body, "Bericht \"final\" ä.pdf")["id"]
        .as_i64()
        .unwrap();
    let p = c
        .get_raw(&format!("/api/nodes/{pdf}/content?inline=true"), &[])
        .await;
    assert_eq!(p.header("content-type"), "application/pdf");
    // The web app may frame previews, nobody else; downloads are never framed.
    assert!(
        p.header("content-security-policy")
            .ends_with("frame-ancestors 'self'")
    );
    assert!(!p.header("content-security-policy").contains("sandbox"));
    assert_eq!(p.header("x-frame-options"), "SAMEORIGIN");
    assert!(v.header("content-security-policy").starts_with("sandbox"));
    assert_eq!(d.header("x-frame-options"), "DENY");
    let pdf_download = c.get_raw(&format!("/api/nodes/{pdf}/content"), &[]).await;
    assert!(
        pdf_download
            .header("content-security-policy")
            .starts_with("sandbox")
    );
    assert_eq!(
        p.header("content-disposition"),
        "inline; filename=\"Bericht _final_ _.pdf\"; filename*=UTF-8''Bericht%20%22final%22%20%C3%A4.pdf"
    );

    // Folders cannot be downloaded, files have no children.
    assert_eq!(
        c.get(&format!("/api/nodes/{fotos}/content")).await.status,
        400
    );
    assert_eq!(
        c.get(&format!("/api/nodes/{txt_id}/children")).await.status,
        400
    );

    // Others see nothing: not even whether the node exists.
    let (mut other, other_root, _, _) = signed_in(&env, "gustav").await;
    for path in [
        format!("/api/nodes/{txt_id}"),
        format!("/api/nodes/{txt_id}/content"),
        format!("/api/nodes/{root_node}/children"),
        "/api/nodes/999999".to_owned(),
    ] {
        assert_eq!(other.get(&path).await.status, 404, "{path}");
    }
    let scan = other
        .post(&format!("/api/roots/{root_id}/scan"), serde_json::json!({}))
        .await;
    assert_eq!(scan.status, 404);
    assert_ne!(other_root, root_id);

    // Signed out: nothing at all.
    let mut anon = env.client();
    assert_eq!(anon.get("/api/roots").await.status, 401);
    assert_eq!(
        anon.get_raw(&format!("/api/nodes/{txt_id}/content"), &[])
            .await
            .status,
        401
    );

    // Deleted on disk: gone after the next scan.
    fs::remove_file(dir.join("Liesmich.txt")).unwrap();
    c.post(&format!("/api/roots/{root_id}/scan"), serde_json::json!({}))
        .await
        .ok();
    assert_eq!(c.get(&format!("/api/nodes/{txt_id}")).await.status, 404);
    env.finish().await;
}

#[test]
fn ablagepfad_nur_mit_normalen_bestandteilen() {
    assert_eq!(
        roots::home_rel_path("homes/{user}/Drive", "klaus").unwrap(),
        Path::new("homes/klaus/Drive")
    );
    for bad in ["../{user}", "/abs/{user}", "homes/../{user}", ""] {
        assert!(roots::home_rel_path(bad, "klaus").is_err(), "{bad}");
    }
}

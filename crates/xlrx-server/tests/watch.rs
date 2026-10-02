//! Partial scans of single folders, and the watcher that triggers them (real inotify).

mod common;

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use common::files::*;
use common::*;
use xlrx_server::files::db::RootRow;
use xlrx_server::files::{roots, watch};

macro_rules! env_or_skip {
    () => {
        match Env::with_data().await {
            Some(e) => e,
            None => return,
        }
    };
}

fn dirs(list: &[&str]) -> Vec<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

fn set(list: &[&str]) -> HashSet<PathBuf> {
    list.iter().map(PathBuf::from).collect()
}

/// Lets timestamps age past the racy window, so partial scans take files right away.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(2100)).await;
}

#[tokio::test]
async fn teil_abgleich_nur_der_genannten_ordner() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "anna").await;
    write(&dir.join("A/x.txt"), b"x");
    write(&dir.join("A/y.txt"), b"y");
    write(&dir.join("A/sub/tief/z.txt"), b"z");
    write(&dir.join("B/b.txt"), b"b");
    settle().await;
    roots::scan(&env.state, &root).await.unwrap();
    let x = node(&env, &root, "A/x.txt").await;

    // Only the folders named: a new file elsewhere waits for its own event.
    write(&dir.join("A/neu.txt"), b"neu");
    write(&dir.join("B/neu.txt"), b"neu");
    settle().await;
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!(r.report.created, 1);
    let all = names(&env, &root).await;
    assert!(all.contains(&"A/neu.txt".to_owned()) && !all.contains(&"B/neu.txt".to_owned()));

    // A move whose halves arrive separately: the source folder alone does not delete it …
    fs::rename(dir.join("A/x.txt"), dir.join("B/x.txt")).unwrap();
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!((r.report.deleted, r.missing_in.clone()), (0, dirs(&["A"])));
    // … the target folder finds it by identity …
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["B"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!(
        (r.report.moved, r.report.created),
        (1, 1),
        "x moved, neu.txt new"
    );
    assert_eq!(node(&env, &root, "B/x.txt").await.id, x.id);
    // … and after the grace period nothing is missing any more.
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&["A"]))
        .await
        .unwrap();
    assert_eq!(r.report.changes(), 0);

    // Really deleted: only once deleting is allowed for that folder, with everything below.
    fs::remove_file(dir.join("A/y.txt")).unwrap();
    fs::remove_dir_all(dir.join("A/sub")).unwrap();
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!(r.report.deleted, 0);
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&["A"]))
        .await
        .unwrap();
    assert_eq!(r.report.deleted, 4, "y.txt, sub, tief, z.txt");
    assert_eq!(
        names(&env, &root).await,
        ["A", "A/neu.txt", "B", "B/b.txt", "B/neu.txt", "B/x.txt"]
    );

    // New folders are read completely.
    write(&dir.join("A/n1/n2/datei.txt"), b"d");
    settle().await;
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!(r.report.created, 3);

    // A folder renamed outside: unknown under its new name until its parent is scanned.
    fs::rename(dir.join("A"), dir.join("A2")).unwrap();
    write(&dir.join("A2/spaeter.txt"), b"s");
    settle().await;
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A2"]), &set(&[]))
        .await
        .unwrap();
    assert_eq!(r.unresolved, dirs(&["A2"]));
    assert_eq!(r.report.moved, 1, "found via its parent");
    let r = roots::scan_dirs(&env.state, &root, &dirs(&["A2"]), &set(&[]))
        .await
        .unwrap();
    assert!(r.unresolved.is_empty());
    assert_eq!(r.report.created, 1);

    // Everything agrees with a full scan.
    let full = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(full.changes(), 0, "{full:?}");
    env.finish().await;
}

#[tokio::test]
async fn teil_abgleich_wartet_auf_fertig_geschriebene_dateien() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "ben").await;
    write(&dir.join("bestand.txt"), b"alt");
    settle().await;
    roots::scan(&env.state, &root).await.unwrap();

    // Just written (maybe still being copied): not taken yet, the folder comes again.
    write(&dir.join("kopie.mov"), b"halb");
    fs::write(dir.join("bestand.txt"), b"neu").unwrap();
    let r = roots::scan_dirs(&env.state, &root, &dirs(&[""]), &set(&[]))
        .await
        .unwrap();
    assert_eq!((r.report.created, r.report.updated), (0, 0));
    assert_eq!(r.busy, dirs(&[""]));
    assert_eq!(names(&env, &root).await, ["bestand.txt"]);

    settle().await;
    let r = roots::scan_dirs(&env.state, &root, &dirs(&[""]), &set(&[]))
        .await
        .unwrap();
    assert_eq!((r.report.created, r.report.updated), (1, 1));
    assert!(r.busy.is_empty());
    env.finish().await;
}

/// Polls until the condition holds (the watcher works in the background).
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..150 {
        if check().await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("nicht eingetreten: {what}");
}

async fn journal_len(env: &Env) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM journal")
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

async fn has(env: &Env, root: &RootRow, path: &str) -> bool {
    names(env, root).await.iter().any(|n| n == path)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aenderungen_von_aussen_erscheinen_von_selbst() {
    let env = env_or_skip!();
    let (root, dir) = home_of(&env, "carla").await;
    write(&dir.join("Projekte/plan.txt"), b"Plan");
    write(&dir.join("Archiv/.keep"), b"");
    settle().await;
    roots::scan(&env.state, &root).await.unwrap();
    watch::start(&env.state, &root).await.unwrap();
    watch::start(&env.state, &root).await.unwrap(); // twice: no second watcher
    let plan = node(&env, &root, "Projekte/plan.txt").await;

    // New file (over SMB, say).
    write(&dir.join("Projekte/neu.txt"), b"neu");
    eventually("neue Datei", || has(&env, &root, "Projekte/neu.txt")).await;

    // Rename and move: same node.
    fs::rename(
        dir.join("Projekte/plan.txt"),
        dir.join("Archiv/plan 2025.txt"),
    )
    .unwrap();
    eventually("verschoben", || has(&env, &root, "Archiv/plan 2025.txt")).await;
    assert_eq!(node(&env, &root, "Archiv/plan 2025.txt").await.id, plan.id);

    // New nested folders, created in one go.
    fs::create_dir_all(dir.join("Fotos/2026/Urlaub")).unwrap();
    write(&dir.join("Fotos/2026/Urlaub/strand.jpg"), b"jpg");
    eventually("neue Ordner", || {
        has(&env, &root, "Fotos/2026/Urlaub/strand.jpg")
    })
    .await;

    // Content changed in place.
    let before = node(&env, &root, "Projekte/neu.txt").await;
    fs::write(dir.join("Projekte/neu.txt"), b"geaendert").unwrap();
    eventually("geändert", || async {
        node(&env, &root, "Projekte/neu.txt").await.content_hash != before.content_hash
    })
    .await;

    // Folder deleted with its content.
    fs::remove_dir_all(dir.join("Fotos")).unwrap();
    eventually("gelöscht", || async { !has(&env, &root, "Fotos").await }).await;
    assert!(!has(&env, &root, "Fotos/2026/Urlaub/strand.jpg").await);

    // Synology's thumbnails and our own temp files are not changes.
    write(
        &dir.join("Archiv/@eaDir/plan 2025.txt/SYNOFILE_THUMB_M.jpg"),
        b"t",
    );
    write(&dir.join(".xlrx-srv-test"), b"t");
    // Quiet now: reading files during scans must not trigger further scans.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let n = journal_len(&env).await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(journal_len(&env).await, n, "keine Änderungen ohne Anlass");

    // And all of it agrees with a full scan.
    let full = roots::scan(&env.state, &root).await.unwrap();
    assert_eq!(full.changes(), 0, "{full:?}");
    env.finish().await;
}

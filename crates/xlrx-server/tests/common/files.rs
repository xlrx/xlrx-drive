//! Helpers for tests with roots and files.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use xlrx_server::files::db::{self, NodeRow, RootRow};
use xlrx_server::files::roots;
use xlrx_server::users;

use super::{Client, Env, setup_with_totp};

/// An account (without sign-in) with its "My Drive".
pub async fn home_of(env: &Env, username: &str) -> (RootRow, PathBuf) {
    let u = users::create(&env.db.pool, username, username, None, false)
        .await
        .expect("Konto");
    let root = roots::ensure_home(&env.state, u.id, username)
        .await
        .expect("Ablage")
        .expect("Datenverzeichnis");
    let dir = env.data_dir().join(format!("homes/{username}/Drive"));
    assert!(dir.is_dir());
    (root, dir)
}

/// Live nodes by path relative to the root directory.
pub async fn tree(env: &Env, root: &RootRow) -> Vec<(String, NodeRow)> {
    let nodes = db::live_nodes(&env.db.pool, root.id).await.unwrap();
    let paths = db::paths(&nodes);
    let mut out: Vec<(String, NodeRow)> = nodes
        .into_iter()
        .filter(|n| n.parent_id.is_some())
        .map(|n| (paths[&n.id].to_string_lossy().into_owned(), n))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

pub async fn node(env: &Env, root: &RootRow, path: &str) -> NodeRow {
    tree(env, root)
        .await
        .into_iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("{path} fehlt"))
        .1
}

pub async fn names(env: &Env, root: &RootRow) -> Vec<String> {
    tree(env, root).await.into_iter().map(|(p, _)| p).collect()
}

pub async fn journal_ops(env: &Env, node_id: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT op FROM journal WHERE node_id = $1 ORDER BY seq")
        .bind(node_id)
        .fetch_all(&env.db.pool)
        .await
        .unwrap()
}

pub fn write(path: &Path, content: &[u8]) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, content).unwrap();
}

/// Replaces a file the way editors save: write a temporary file, rename it over the original.
pub fn atomic_save(path: &Path, content: &[u8]) {
    let tmp = path.with_file_name(".speichern.tmp");
    fs::write(&tmp, content).unwrap();
    fs::rename(&tmp, path).unwrap();
}

/// Signs in a fresh account (password + TOTP) and returns the client, root id, root node id and
/// root directory.
pub async fn signed_in(env: &Env, username: &str) -> (Client, i64, i64, PathBuf) {
    let invite = env.invite(username, false).await;
    let (mut c, _, _) = setup_with_totp(env, &invite).await;
    let roots = c.get("/api/roots").await;
    let r = &roots.ok()[0];
    assert_eq!(r["name"], "Meine Ablage");
    let dir = env.data_dir().join(format!("homes/{username}/Drive"));
    (
        c,
        r["id"].as_i64().unwrap(),
        r["node_id"].as_i64().unwrap(),
        dir,
    )
}

pub fn find<'a>(list: &'a Value, name: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|n| n["name"] == name)
        .unwrap_or_else(|| panic!("{name} fehlt in {list}"))
}

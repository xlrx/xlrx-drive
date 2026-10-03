//! Start page: files changed last, newest first, only one's own, with their folder.

mod common;

use common::files::*;
use common::*;
use serde_json::json;

#[tokio::test]
async fn zuletzt_geaendert() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    let (mut c, root_id, _, dir) = signed_in(&env, "anna").await;
    write(&dir.join("alt.txt"), b"alt");
    write(&dir.join("Belege/neu.txt"), b"neu");
    write(&dir.join("Belege/Ordner/tief.txt"), b"tief");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    std::fs::File::options()
        .write(true)
        .open(dir.join("alt.txt"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    c.post(&format!("/api/roots/{root_id}/scan"), json!({}))
        .await
        .ok();
    let list = c.get("/api/recent?limit=10").await.ok().clone();
    let names: Vec<_> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["name"].as_str().unwrap(), n["folder"].as_str().unwrap()))
        .collect();
    assert_eq!(names.len(), 3, "nur Dateien: {names:?}");
    assert_eq!(names[2], ("alt.txt", "Meine Ablage"));
    assert!(names.contains(&("tief.txt", "Meine Ablage/Belege/Ordner")));
    let (mut bert, _, _, _) = signed_in(&env, "bert").await;
    assert_eq!(bert.get("/api/recent").await.ok(), &json!([]));
    env.finish().await;
}

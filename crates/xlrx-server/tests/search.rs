//! Full-text search: word stems and word beginnings, rights, changes showing up at once, syntax,
//! snippets, and an index that survives restarts or is rebuilt when lost.

mod common;

use common::files::*;
use common::*;
use serde_json::{Value, json};
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::roots;
use xlrx_server::{AppState, search};

fn encode(q: &str) -> String {
    url::form_urlencoded::byte_serialize(q.as_bytes()).collect()
}

async fn find(c: &mut Client, q: &str) -> Value {
    c.get(&format!("/api/search?q={}", encode(q)))
        .await
        .ok()
        .clone()
}

fn names(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

/// Stores the text of a file as extraction would.
async fn text_of(env: &Env, root: &RootRow, path: &str, text: &str) {
    let n = node(env, root, path).await;
    let hash: [u8; 32] = n.content_hash.unwrap().try_into().unwrap();
    search::text::store(&env.db.pool, &hash, "plain", text)
        .await
        .unwrap();
}

async fn stop(env: &Env) {
    env.state.search.get().unwrap().stop().await;
}

#[tokio::test]
async fn namen_wortstamm_wortanfang_und_rechte() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut anna, root_id, _, dir) = signed_in(&env, "anna").await;
    write(&dir.join("Rechnung Heizung 2025.pdf"), b"%PDF");
    write(&dir.join("Notizen/Einkaufsliste.txt"), b"Milch");
    write(&dir.join("Häuser am See.txt"), b"See");
    write(&dir.join("Straßenfest.txt"), b"Fest");
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();
    let (mut bert, bert_root, _, bert_dir) = signed_in(&env, "bert").await;
    write(&bert_dir.join("Rechnung Bert.pdf"), b"%PDF");
    roots::scan(&env.state, &root(&env, bert_root).await)
        .await
        .unwrap();

    // Word stems: plural finds singular, umlauts and ß fold.
    assert_eq!(
        names(&find(&mut anna, "Rechnungen").await),
        ["Rechnung Heizung 2025.pdf"]
    );
    assert_eq!(names(&find(&mut anna, "haus").await), ["Häuser am See.txt"]);
    assert_eq!(
        names(&find(&mut anna, "strassenfest").await),
        ["Straßenfest.txt"]
    );
    // Word beginnings while typing, several words all must match.
    assert_eq!(
        names(&find(&mut anna, "Rech Heiz").await),
        ["Rechnung Heizung 2025.pdf"]
    );
    assert!(names(&find(&mut anna, "Rech Keller").await).is_empty());
    // Folders are found too; the folder of a hit is named.
    let r = find(&mut anna, "notizen").await;
    assert_eq!(r["hits"][0]["kind"], "dir");
    let r = find(&mut anna, "einkaufsliste").await;
    assert_eq!(r["hits"][0]["folder"], "Meine Ablage/Notizen");
    assert_eq!(r["total"], 1);
    assert_eq!(r["pending"], 0);

    // Only one's own files, never someone else's.
    assert_eq!(
        names(&find(&mut bert, "rechnung").await),
        ["Rechnung Bert.pdf"]
    );
    let r = find(&mut anna, "rechnung").await;
    assert_eq!(names(&r), ["Rechnung Heizung 2025.pdf"]);
    // Not even counted (the index itself only looks at readable roots).
    assert_eq!(r["total"], 1);
    assert!(names(&find(&mut bert, "einkaufsliste").await).is_empty());
    // Also not by naming someone else's folder, nor by searching in it.
    let bert_node = node(&env, &root(&env, bert_root).await, "Rechnung Bert.pdf").await;
    let r = anna
        .get(&format!(
            "/api/search?q=rechnung&folder={}",
            bert_node.parent_id.unwrap()
        ))
        .await;
    assert_eq!(r.status, 404);

    // Second line of defence: a hit the index still has but the database no longer allows is
    // dropped (here: deleted behind the index's back, without a journal entry).
    let fest = node(&env, &root(&env, root_id).await, "Straßenfest.txt").await;
    sqlx::query("UPDATE nodes SET deleted_at = now() WHERE id = $1")
        .bind(fest.id)
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = find(&mut anna, "strassenfest").await;
    assert_eq!(r["total"], 1);
    assert_eq!(r["hits"], json!([]));

    // Suggestions: names starting like the words typed, with the matched part marked.
    let s = anna
        .get(&format!("/api/search/suggest?q={}", encode("einkau")))
        .await
        .ok()
        .clone();
    assert_eq!(s[0]["name"], "Einkaufsliste.txt");
    assert_eq!(
        s[0]["parts"],
        json!([{"text": "Einkau", "hit": true}, {"text": "fsliste.txt", "hit": false}])
    );
    let s = bert
        .get(&format!("/api/search/suggest?q={}", encode("einkau")))
        .await
        .ok()
        .clone();
    assert_eq!(s, json!([]));
    // Half-typed filters are no error while typing.
    let s = anna
        .get(&format!("/api/search/suggest?q={}", encode("nach:20")))
        .await
        .ok()
        .clone();
    assert_eq!(s, json!([]));
    stop(&env).await;
    env.finish().await;
}

#[tokio::test]
async fn aenderungen_sind_sofort_auffindbar() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut c, root_id, root_node, dir) = signed_in(&env, "carla").await;
    write(&dir.join("Projekte/Bauplan.txt"), b"Plan");
    write(&dir.join("Projekte/Unter/Skizze.txt"), b"Skizze");
    write(&dir.join("entwurf.txt"), b"Entwurf");
    let r = root(&env, root_id).await;
    roots::scan(&env.state, &r).await.unwrap();
    let archiv = c
        .post(
            &format!("/api/nodes/{root_node}/folders"),
            json!({"name": "Archiv"}),
        )
        .await
        .ok()["id"]
        .as_i64()
        .unwrap();

    // Rename a file: the new name is found at once, the old one no more.
    let entwurf = node(&env, &r, "entwurf.txt").await;
    c.send(
        "PATCH",
        &format!("/api/nodes/{}", entwurf.id),
        Some(json!({"name": "Angebot Dach.txt"})),
    )
    .await
    .ok();
    assert_eq!(names(&find(&mut c, "angebot").await), ["Angebot Dach.txt"]);
    assert!(names(&find(&mut c, "entwurf").await).is_empty());

    // Move a folder: everything below it is now in the target folder.
    assert!(names(&find(&mut c, "skizze in:Archiv").await).is_empty());
    let projekte = node(&env, &r, "Projekte").await;
    c.send(
        "PATCH",
        &format!("/api/nodes/{}", projekte.id),
        Some(json!({"parent_id": archiv})),
    )
    .await
    .ok();
    assert_eq!(
        names(&find(&mut c, "skizze in:Archiv").await),
        ["Skizze.txt"]
    );
    assert_eq!(
        names(&find(&mut c, "in:archiv typ:text").await),
        ["Bauplan.txt", "Skizze.txt"]
    );
    assert_eq!(
        names(&find(&mut c, "typ:text -in:Projekte").await),
        ["Angebot Dach.txt"]
    );
    // Renaming a folder keeps its contents findable under the new name (no subtree refresh).
    c.send(
        "PATCH",
        &format!("/api/nodes/{archiv}"),
        Some(json!({"name": "Altes Archiv"})),
    )
    .await
    .ok();
    assert_eq!(
        names(&find(&mut c, "skizze in:\"Altes Archiv\"").await),
        ["Skizze.txt"]
    );
    let reparented: Vec<Option<bool>> = sqlx::query_scalar(
        "SELECT reparented FROM journal WHERE node_id = $1 AND op = 'move' ORDER BY seq",
    )
    .bind(archiv)
    .fetch_all(&env.db.pool)
    .await
    .unwrap();
    assert_eq!(reparented, [Some(false)]);
    // Within a folder only.
    let r2 = c
        .get(&format!("/api/search?q=typ:text&folder={}", projekte.id))
        .await
        .ok()
        .clone();
    assert_eq!(names(&r2), ["Bauplan.txt", "Skizze.txt"]);

    // Trash and back.
    c.send("DELETE", &format!("/api/nodes/{}", projekte.id), None)
        .await
        .ok();
    assert!(names(&find(&mut c, "skizze").await).is_empty());
    assert!(names(&find(&mut c, "projekte").await).is_empty());
    c.post(&format!("/api/trash/{}/restore", projekte.id), json!({}))
        .await
        .ok();
    assert_eq!(names(&find(&mut c, "skizze").await), ["Skizze.txt"]);

    // Changes on disk (SMB, File Station) arrive through the scan the same way.
    std::fs::rename(dir.join("Angebot Dach.txt"), dir.join("Angebot Garage.txt")).unwrap();
    roots::scan(&env.state, &r).await.unwrap();
    assert_eq!(names(&find(&mut c, "garage").await), ["Angebot Garage.txt"]);
    assert!(names(&find(&mut c, "dach").await).is_empty());
    stop(&env).await;
    env.finish().await;
}

#[tokio::test]
async fn volltext_syntax_und_ausschnitte() {
    let Some(env) = Env::with_data().await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut c, root_id, _, dir) = signed_in(&env, "dora").await;
    write(&dir.join("a.txt"), b"a");
    write(&dir.join("b.txt"), b"b");
    write(&dir.join("Scan.pdf"), b"%PDF-1");
    write(&dir.join("Kopie von a.txt"), b"a");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    text_of(
        &env,
        &home,
        "a.txt",
        "Sehr geehrte Damen und Herren,\n\nanbei die Rechnung für die Heizung im Keller. \
         Die Wartung wurde am Montag durchgeführt und ist bis zum Ende des Monats zu bezahlen.",
    )
    .await;
    text_of(
        &env,
        &home,
        "b.txt",
        "Dear Sir or Madam, please find attached the invoice for the heating in the basement. \
         The maintenance was carried out on Monday and is due at the end of the month.",
    )
    .await;
    text_of(&env, &home, "Scan.pdf", "Kontoauszug Sparkasse").await;

    // Text in German and English, by word stem; duplicates of a content both show up.
    assert_eq!(
        names(&find(&mut c, "Heizungen").await),
        ["Kopie von a.txt", "a.txt"]
    );
    assert_eq!(names(&find(&mut c, "invoices").await), ["b.txt"]);
    let hit = find(&mut c, "Heizungen").await["hits"][0].clone();
    let parts = hit["snippet"].as_array().unwrap();
    assert!(
        parts
            .iter()
            .any(|p| p["hit"] == true && p["text"] == "Heizung"),
        "{parts:?}"
    );
    assert!(
        parts
            .iter()
            .all(|p| !p["text"].as_str().unwrap().contains('\n')),
        "{parts:?}"
    );
    // Phrases, exclusions.
    assert_eq!(
        names(&find(&mut c, "„Heizung im Keller“ -Kopie").await),
        ["a.txt"]
    );
    assert!(names(&find(&mut c, "\"Keller im Heizung\"").await).is_empty());
    assert!(names(&find(&mut c, "Rechnung -Keller").await).is_empty());
    // Kinds, with counts per kind regardless of the kind chosen.
    let pdf = find(&mut c, "typ:pdf").await;
    assert_eq!(names(&pdf), ["Scan.pdf"]);
    let facets = pdf["facets"].as_array().unwrap();
    assert!(
        facets.contains(&json!({"typ": "pdf", "n": 1})),
        "{facets:?}"
    );
    assert!(
        facets.contains(&json!({"typ": "text", "n": 3})),
        "{facets:?}"
    );
    assert_eq!(
        names(&find(&mut c, "typ:txt -typ:pdf kontoauszug").await),
        Vec::<String>::new()
    );
    assert_eq!(names(&find(&mut c, "kontoauszug").await), ["Scan.pdf"]);
    // Dates.
    assert_eq!(find(&mut c, "nach:2000 typ:text").await["total"], 3);
    assert_eq!(find(&mut c, "vor:2000-01-01 typ:text").await["total"], 0);
    let bad = c
        .get(&format!("/api/search?q={}", encode("nach:gestern")))
        .await;
    assert_eq!(bad.status, 400);
    assert!(bad.err().contains("nach:"));
    // A typo: similar spellings, marked as such.
    let r = find(&mut c, "Rechnnug").await;
    assert_eq!(r["fuzzy"], true);
    assert!(names(&r).contains(&"a.txt".into()));
    // Nothing typed: nothing found.
    assert_eq!(find(&mut c, "  ").await["total"], 0);
    assert_eq!(find(&mut c, "!!!").await["total"], 0);
    // New text for a content arrives in the index without any change to the file.
    text_of(&env, &home, "Scan.pdf", "Mietvertrag Wohnung").await;
    let mut found = Vec::new();
    for _ in 0..100 {
        found = names(&find(&mut c, "mietvertrag").await);
        if !found.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert_eq!(found, ["Scan.pdf"]);
    stop(&env).await;
    env.finish().await;
}

#[tokio::test]
async fn index_ueberlebt_neustart_und_wird_neu_aufgebaut() {
    let Some(mut env) = Env::with_data().await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut c, root_id, _, dir) = signed_in(&env, "emil").await;
    write(&dir.join("Steuererklärung.pdf"), b"%PDF");
    roots::scan(&env.state, &root(&env, root_id).await)
        .await
        .unwrap();
    assert_eq!(
        names(&find(&mut c, "steuererklarung").await),
        ["Steuererklärung.pdf"]
    );
    let indexed = env.state.search.get().unwrap().indexed();
    assert!(indexed > 0);

    // Restart: continues from its last commit, nothing is read again.
    stop(&env).await;
    let mut c = restart(&mut env, &c);
    search::start(&env.state).await.unwrap();
    assert_eq!(env.state.search.get().unwrap().indexed(), indexed);
    assert_eq!(
        names(&find(&mut c, "steuererklarung").await),
        ["Steuererklärung.pdf"]
    );

    // Lost or damaged: built again from the database.
    let index_dir = env.state.cfg.state_dir.clone().unwrap().join("index");
    for damage in [
        |d: &std::path::Path| std::fs::remove_dir_all(d).unwrap(),
        |d: &std::path::Path| std::fs::write(d.join("meta.json"), b"{kaputt").unwrap(),
    ] {
        stop(&env).await;
        damage(&index_dir);
        c = restart(&mut env, &c);
        search::start(&env.state).await.unwrap();
        assert_eq!(env.state.search.get().unwrap().indexed(), 0);
        assert_eq!(
            names(&find(&mut c, "steuererklarung").await),
            ["Steuererklärung.pdf"]
        );
    }
    stop(&env).await;
    env.finish().await;
}

/// A new server on the same database and directories, signed in as before.
fn restart(env: &mut Env, c: &Client) -> Client {
    env.state = AppState::new(env.db.pool.clone(), env.state.cfg.clone()).unwrap();
    env.app = xlrx_server::router(env.state.clone());
    let mut again = env.client();
    again.cookie = c.cookie.clone();
    again
}

//! AI search, M4.3: hybrid search. Hits by meaning (texts in the cloud and in the home network,
//! pictures by description and by CLIP) join the lexical ones by rank, with the same rights and
//! filters; a provider that does not answer leaves only its list out.

mod common;

use std::time::Duration;

use common::ai::FakeAi;
use common::ai::*;
use common::files::*;
use common::*;
use serde_json::Value;
use xlrx_server::files::data_class::Class;
use xlrx_server::search;

const HEIZUNG: &str = "Rechnung der Firma Müller über die Wartung der Heizung im November. \
                       Bitte überweisen Sie den Betrag innerhalb von 14 Tagen.";
const URLAUB: &str = "Packliste für den Urlaub am Strand: Sonnencreme, Handtuch, Badehose, \
                      Sonnenbrille und ein gutes Buch.";
const BEFUND: &str = "Befund der Blutwerte vom Arzt: Cholesterin leicht erhöht, \
                      Kontrolle in drei Monaten empfohlen.";

async fn find(c: &mut Client, q: &str) -> Value {
    let q: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    c.get(&format!("/api/search?q={q}")).await.ok().clone()
}

/// (name, how found by meaning) of the hits.
fn hits(r: &Value) -> Vec<(String, Option<String>)> {
    r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["name"].as_str().unwrap().to_string(),
                h["by"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn names(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = hits(r).into_iter().map(|h| h.0).collect();
    v.sort();
    v
}

fn snippet(h: &Value) -> String {
    h["snippet"]
        .as_array()
        .map(|p| p.iter().map(|x| x["text"].as_str().unwrap()).collect())
        .unwrap_or_default()
}

/// Waits until the index has taken in the files (names are found).
async fn indexed(c: &mut Client, name_word: &str) {
    for _ in 0..100 {
        if !names(&find(c, name_word).await).is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{name_word} nicht im Index");
}

#[tokio::test]
async fn bedeutung_und_woerter_gemischt_mit_rechten_und_filtern() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Arbeit/Wartung.txt", HEIZUNG),
            ("Arbeit/Packliste.txt", URLAUB),
            ("Privat/Labor.txt", BEFUND),
        ],
    )
    .await;
    let arbeit = node(&env, &home, "Arbeit").await.id;
    set_class(&mut klaus, arbeit, "cloud").await;
    start_cloud(&env).await;
    plan(&env).await;
    work(&env).await;
    indexed(&mut klaus, "Packliste").await;

    // Only the meaning matches: "Therme" is nowhere in the text.
    let r = find(&mut klaus, "Therme").await;
    assert_eq!(
        hits(&r),
        vec![("Wartung.txt".to_string(), Some("bedeutung".to_string()))]
    );
    assert_eq!(r["total"], 1);
    assert!(snippet(&r["hits"][0]).starts_with("Rechnung der Firma Müller"));
    assert_eq!(r["facets"][0]["n"], 1);
    // From the home network's space ("Nur lokal").
    assert_eq!(
        hits(&find(&mut klaus, "Doktor").await),
        vec![("Labor.txt".to_string(), Some("bedeutung".to_string()))]
    );
    // Words found it: no mark, the words highlighted.
    let r = find(&mut klaus, "Heizung").await;
    assert_eq!(hits(&r), vec![("Wartung.txt".to_string(), None)]);
    // Found by words and by meaning: counted once.
    assert_eq!(r["total"], 1);
    assert_eq!(r["facets"][0]["n"], 1);
    assert!(
        r["hits"][0]["snippet"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["hit"] == true)
    );
    // Both kinds in one list: the words find the invoice, the meaning the packing list.
    let r = find(&mut klaus, "Rechnung Ferien").await;
    assert_eq!(names(&r), ["Packliste.txt", "Wartung.txt"]);
    assert_eq!(r["total"], 2);
    // Paging over the merged list.
    let all = hits(&r);
    for (i, h) in all.iter().enumerate() {
        let one = klaus
            .get(&format!(
                "/api/search?q=Rechnung%20Ferien&offset={i}&limit=1"
            ))
            .await
            .ok()
            .clone();
        assert_eq!(hits(&one), vec![h.clone()]);
        assert_eq!(one["total"], 2);
    }

    // Filters and exclusions apply to hits by meaning as well.
    assert!(names(&find(&mut klaus, "Therme typ:bild").await).is_empty());
    assert_eq!(
        names(&find(&mut klaus, "Therme typ:text").await),
        ["Wartung.txt"]
    );
    assert_eq!(
        names(&find(&mut klaus, "Therme in:Arbeit").await),
        ["Wartung.txt"]
    );
    assert!(names(&find(&mut klaus, "Therme in:Privat").await).is_empty());
    assert!(names(&find(&mut klaus, "Therme -Müller").await).is_empty());
    assert!(names(&find(&mut klaus, "Therme nach:2100").await).is_empty());
    // A phrase in quotes must be there word for word.
    assert_eq!(
        names(&find(&mut klaus, "Therme \"Firma Müller\"").await),
        ["Wartung.txt"]
    );
    assert!(names(&find(&mut klaus, "Therme \"Firma Schmidt\"").await).is_empty());
    // Within a folder.
    let privat = node(&env, &home, "Privat").await.id;
    let r = klaus
        .get(&format!("/api/search?q=Therme&folder={privat}"))
        .await
        .ok()
        .clone();
    assert!(names(&r).is_empty());

    // Someone else finds only what they may see.
    let (mut anna, anna_root, _, anna_dir) = signed_in(&env, "anna").await;
    let anna_home = root(&env, anna_root).await;
    files(
        &env,
        &anna_home,
        &anna_dir,
        &[(
            "Notiz.txt",
            "Der Heizkessel im Keller tropft seit gestern wieder.",
        )],
    )
    .await;
    let notiz = node(&env, &anna_home, "Notiz.txt").await;
    set_class(&mut anna, notiz.parent_id.unwrap(), "cloud").await;
    plan(&env).await;
    work(&env).await;
    indexed(&mut anna, "Notiz").await;
    assert_eq!(names(&find(&mut anna, "Therme").await), ["Notiz.txt"]);
    assert!(names(&find(&mut anna, "Doktor").await).is_empty());
    assert_eq!(names(&find(&mut klaus, "Therme").await), ["Wartung.txt"]);

    // The provider is gone: the lexical search answers all the same.
    fake.fail_with(Some(503)).await;
    assert_eq!(
        names(&find(&mut klaus, "Heizung Rechnung").await),
        ["Wartung.txt"]
    );
    assert!(names(&find(&mut klaus, "Heizkessel").await).is_empty());
    fake.fail_with(None).await;
    // A query is embedded once (paging, typing it again).
    fake.clear().await;
    find(&mut klaus, "Ferien").await;
    find(&mut klaus, "Ferien").await;
    let queries = fake
        .seen()
        .await
        .iter()
        .filter(|s| s.inputs().iter().any(|i| i.contains("Ferien")))
        .count();
    // Cloud and home network: once each.
    assert_eq!(queries, 2);
    let (calls,): (i64,) = sqlx::query_as("SELECT calls FROM ai_usage WHERE kind = 'query'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert!(calls >= 1);
    env.finish().await;
}

#[tokio::test]
async fn bilder_nach_bedeutung_und_bildinhalt() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    write(
        &dir.join("Fotos/IMG_0001.jpg"),
        &photo(1600, 1200, [200, 40, 30], false),
    );
    write(
        &dir.join("Privat/IMG_0002.jpg"),
        &photo(1200, 900, [210, 30, 40], false),
    );
    write(
        &dir.join("Fotos/Scan_0003.jpg"),
        &photo(800, 1100, [30, 50, 210], false),
    );
    xlrx_server::files::roots::scan(&env.state, &home)
        .await
        .unwrap();
    let fotos = node(&env, &home, "Fotos").await.id;
    set_class(&mut klaus, fotos, "cloud").await;
    start_cloud(&env).await;
    plan(&env).await;
    work(&env).await;
    indexed(&mut klaus, "IMG").await;

    // "Welpe": the cloud's description of one picture (by meaning), the other picture itself
    // (CLIP, "Nur lokal").
    let r = find(&mut klaus, "Welpe").await;
    let mut h = hits(&r);
    h.sort();
    assert_eq!(
        h,
        vec![
            ("IMG_0001.jpg".to_string(), Some("bild".to_string())),
            ("IMG_0002.jpg".to_string(), Some("bild".to_string())),
        ]
    );
    let first = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .find(|h| h["name"] == "IMG_0001.jpg")
        .unwrap();
    assert!(snippet(first).contains("Hund"));
    // The words of a description are found lexically (once the index took them in), the kind
    // of document filters.
    for _ in 0..100 {
        if !names(&find(&mut klaus, "Heizung dokument:rechnung").await).is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        names(&find(&mut klaus, "Heizung dokument:rechnung").await),
        ["Scan_0003.jpg"]
    );
    assert_eq!(
        hits(&find(&mut klaus, "Strand dokument:rechnung").await),
        vec![]
    );
    // The words find the described picture first; CLIP adds the one from "Nur lokal".
    assert_eq!(
        hits(&find(&mut klaus, "Strand").await),
        vec![
            ("IMG_0001.jpg".to_string(), None),
            ("IMG_0002.jpg".to_string(), Some("bild".to_string()))
        ]
    );
    env.finish().await;
}

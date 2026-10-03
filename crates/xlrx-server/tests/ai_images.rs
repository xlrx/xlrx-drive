//! AI search, M4.2: pictures. "Cloud erlaubt": one vision call per picture, sent scaled down,
//! without EXIF/GPS and without names; the description is embedded and found by the full-text
//! search (also `dokument:`). "Nur lokal": CLIP vectors in the home network.

mod common;

use std::time::Duration;

use common::ai::*;
use common::ai::{self as fake, FakeAi};
use common::files::*;
use common::*;
use xlrx_server::ai::{Space, pipeline, vectors};
use xlrx_server::files::data_class::Class;
use xlrx_server::files::roots;
use xlrx_server::search;

const RED: [u8; 3] = [200, 40, 30];
const BLUE: [u8; 3] = [30, 50, 210];

/// Writes pictures and reads them in.
async fn pictures(
    env: &Env,
    root: &xlrx_server::files::db::RootRow,
    dir: &std::path::Path,
    list: &[(&str, Vec<u8>)],
) {
    for (path, bytes) in list {
        write(&dir.join(path), bytes);
    }
    roots::scan(&env.state, root).await.unwrap();
}

async fn search_names(c: &mut Client, q: &str) -> Vec<String> {
    let q: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    let r = c.get(&format!("/api/search?q={q}")).await.ok().clone();
    let mut v: Vec<String> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["name"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

/// Waits until a search finds what is expected (the index follows within moments).
async fn eventually(c: &mut Client, q: &str, want: &[&str]) {
    let mut found = Vec::new();
    for _ in 0..100 {
        found = search_names(c, q).await;
        if found == want {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("„{q}“: {found:?} statt {want:?}");
}

/// The pictures sent to the vision model.
async fn sent_pictures(fake: &FakeAi) -> Vec<(Vec<u8>, image::RgbImage)> {
    fake.seen()
        .await
        .iter()
        .filter(|s| s.path == "/v1/chat/completions")
        .map(|s| {
            picture_bytes(
                s.body["messages"][1]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap(),
            )
        })
        .collect()
}

async fn vision_of(env: &Env, h: &[u8]) -> Option<(String, Option<String>, Option<time::Date>)> {
    sqlx::query_as(
        "SELECT description, doc_type, date_found FROM ai_vision WHERE content_hash = $1",
    )
    .bind(h)
    .fetch_optional(&env.db.pool)
    .await
    .unwrap()
}

/// (space, model, source) of a content's vectors.
async fn sources_of(env: &Env, h: &[u8]) -> Vec<(String, String, String)> {
    sqlx::query_as(
        "SELECT space, model, source FROM ai_vectors WHERE content_hash = $1 ORDER BY space, source",
    )
    .bind(h)
    .fetch_all(&env.db.pool)
    .await
    .unwrap()
}

fn row(space: &str, model: &str, source: &str) -> (String, String, String) {
    (space.into(), model.into(), source.into())
}

#[tokio::test]
async fn bilder_in_der_cloud_beschrieben_im_heimnetz_als_clip() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    pictures(
        &env,
        &home,
        &dir,
        &[
            ("Fotos/IMG_0001.jpg", photo(2000, 1500, RED, true)),
            ("Fotos/Scan_0002.jpg", photo(800, 1100, BLUE, false)),
            ("Privat/IMG_0003.jpg", photo(900, 1200, RED, true)),
        ],
    )
    .await;
    let fotos = node(&env, &home, "Fotos").await.id;
    set_class(&mut klaus, fotos, "cloud").await;
    start_cloud(&env).await;
    let strand = hash(&env, &home, "Fotos/IMG_0001.jpg").await;
    let scan = hash(&env, &home, "Fotos/Scan_0002.jpg").await;
    let privat = hash(&env, &home, "Privat/IMG_0003.jpg").await;

    plan(&env).await;
    let mut want = vec![
        (
            "ai_cloud".to_string(),
            image_key(&strand),
            "queued".to_string(),
            0,
        ),
        ("ai_cloud".into(), image_key(&scan), "queued".into(), 0),
        ("ai_local".into(), image_key(&privat), "queued".into(), 0),
    ];
    want.sort();
    assert_eq!(jobs(&env).await, want);
    work(&env).await;
    assert!(jobs(&env).await.is_empty());

    // The cloud saw the two pictures of "Fotos": scaled down, without EXIF and GPS.
    let sent = sent_pictures(&fake).await;
    assert_eq!(sent.len(), 2);
    for (bytes, img) in &sent {
        assert!(
            img.width().max(img.height()) <= 1024,
            "{}x{}",
            img.width(),
            img.height()
        );
        let raw = String::from_utf8_lossy(bytes);
        assert!(!raw.contains("GPSGEHEIM") && !raw.contains("Exif"));
    }
    assert!(
        sent.iter()
            .any(|(_, i)| (i.width(), i.height()) == (1024, 768))
    );
    let all = fake.sent_text().await;
    for name in [
        "IMG_0001",
        "Scan_0002",
        "IMG_0003",
        "Fotos",
        "Privat",
        "klaus",
    ] {
        assert!(!all.contains(name), "{name} gesendet");
    }
    // The picture from "Privat" only went to the home network, as CLIP input.
    let seen = fake.seen().await;
    let clip: Vec<_> = seen
        .iter()
        .filter(|s| s.body["modality"] == "image")
        .collect();
    assert_eq!(clip.len(), 1);
    assert!(clip[0].auth.is_none());
    assert_eq!(clip[0].body["model"], fake::CLIP_MODEL);
    assert!(
        seen.iter()
            .filter(|s| s.auth.is_some())
            .all(|s| s.body["modality"].is_null())
    );

    // What was seen is stored, embedded, and costs are counted.
    let (description, doc, date) = vision_of(&env, &scan).await.unwrap();
    assert!(description.contains("Heizung"));
    assert_eq!(doc.as_deref(), Some("rechnung"));
    assert_eq!(date, Some(time::macros::date!(2025 - 11 - 14)));
    assert_eq!(vision_of(&env, &strand).await.unwrap().1, None);
    assert_eq!(vision_of(&env, &privat).await, None);
    assert_eq!(
        sources_of(&env, &strand).await,
        vec![row("cloud", "qwen3-embedding-8b", "description")]
    );
    assert_eq!(
        sources_of(&env, &privat).await,
        vec![row("clip", fake::CLIP_MODEL, "image")]
    );
    let (calls, tin, tout, cost): (i64, i64, i64, f64) = sqlx::query_as(
        "SELECT calls, tokens_in, tokens_out, cost FROM ai_usage WHERE kind = 'vision'",
    )
    .fetch_one(&env.db.pool)
    .await
    .unwrap();
    assert_eq!((calls, tin, tout), (2, 560, 120));
    assert!((cost - (560.0 * 0.25 + 120.0 * 0.50) / 1e6).abs() < 1e-12);

    // Found by meaning: the description in the cloud space, the picture itself with CLIP.
    assert_eq!(
        nearest(&env, Space::Cloud, "Hund am Strand").await[0],
        strand
    );
    assert_eq!(
        nearest(&env, Space::Cloud, "Rechnung Heizung").await[0],
        scan
    );
    let (model, dim) = env.state.ai.model(Space::Clip).unwrap();
    let q = env
        .state
        .ai
        .local
        .as_ref()
        .unwrap()
        .clip_query("Hund am Strand", Duration::from_secs(5))
        .await
        .unwrap();
    let near = vectors::nearest(&env.db.pool, Space::Clip, model, dim, &q.vectors[0], 5)
        .await
        .unwrap();
    assert_eq!(near[0].content_hash, privat);

    // The full-text search finds pictures by what they show, by the text in them and by kind.
    // (The picture from "Privat" joins by its CLIP vector.)
    eventually(&mut klaus, "Strand", &["IMG_0001.jpg", "IMG_0003.jpg"]).await;
    eventually(&mut klaus, "Heizungswartung", &["Scan_0002.jpg"]).await;
    eventually(&mut klaus, "dokument:rechnung", &["Scan_0002.jpg"]).await;
    eventually(
        &mut klaus,
        "-dokument:rechnung Hund",
        &["IMG_0001.jpg", "IMG_0003.jpg"],
    )
    .await;
    let r = klaus.get("/api/search?q=Strand").await.ok().clone();
    let snippet: String = r["hits"][0]["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap())
        .collect();
    assert!(snippet.contains("Strand"), "{snippet}");

    // Once per content: a copy costs nothing.
    fake.clear().await;
    pictures(
        &env,
        &home,
        &dir,
        &[("Fotos/Kopie.jpg", photo(2000, 1500, RED, true))],
    )
    .await;
    plan(&env).await;
    work(&env).await;
    assert!(fake.seen().await.is_empty());
    env.finish().await;
}

#[tokio::test]
async fn nur_lokal_nimmt_die_bildbeschreibung_zurueck() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    search::start(&env.state).await.unwrap();
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    pictures(
        &env,
        &home,
        &dir,
        &[("Fotos/IMG_0001.jpg", photo(1600, 1200, RED, false))],
    )
    .await;
    let fotos = node(&env, &home, "Fotos").await.id;
    set_class(&mut klaus, fotos, "cloud").await;
    start_cloud(&env).await;
    plan(&env).await;
    work(&env).await;
    let strand = hash(&env, &home, "Fotos/IMG_0001.jpg").await;
    eventually(&mut klaus, "Strand", &["IMG_0001.jpg"]).await;

    // "Nur lokal": description and its vector go, also from the full-text search; the home
    // network makes a CLIP vector instead.
    set_class(&mut klaus, fotos, "local").await;
    assert_eq!(vision_of(&env, &strand).await, None);
    assert!(sources_of(&env, &strand).await.is_empty());
    eventually(&mut klaus, "Strand", &[]).await;
    assert_eq!(
        jobs(&env).await,
        vec![(
            "ai_local".to_string(),
            image_key(&strand),
            "queued".to_string(),
            0
        )]
    );
    fake.clear().await;
    work(&env).await;
    assert_eq!(
        sources_of(&env, &strand).await,
        vec![row("clip", fake::CLIP_MODEL, "image")]
    );
    assert!(fake.seen().await.iter().all(|s| s.auth.is_none()));

    // Allowed again: described again, the CLIP vector goes.
    set_class(&mut klaus, fotos, "cloud").await;
    work(&env).await;
    assert_eq!(
        sources_of(&env, &strand).await,
        vec![row("cloud", "qwen3-embedding-8b", "description")]
    );
    eventually(&mut klaus, "Strand", &["IMG_0001.jpg"]).await;

    // A file gone for good: its description leaves the index too.
    std::fs::remove_file(dir.join("Fotos/IMG_0001.jpg")).unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    assert_eq!(pipeline::sweep(&env.state).await.unwrap().removed, 1);
    assert_eq!(vision_of(&env, &strand).await, None);
    let log: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_vision_log WHERE content_hash = $1")
        .bind(&strand)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(log, 4);
    env.finish().await;
}

#[tokio::test]
async fn kleine_kaputte_und_bekannte_bilder() {
    let fake = FakeAi::start().await;
    let Some(mut env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let (_, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    let garbage: Vec<u8> = (0..30_000u32).map(|i| (i * 7919 % 251) as u8).collect();
    pictures(
        &env,
        &home,
        &dir,
        &[
            // An icon: too small a file to be looked at at all.
            ("icon.png", photo(16, 16, BLUE, false)),
            // Large enough as a file, but a small picture.
            ("klein.jpg", photo(180, 120, BLUE, false)),
            // Not a picture at all.
            ("kaputt.jpg", garbage),
            ("IMG_0001.jpg", photo(1600, 1200, RED, false)),
        ],
    )
    .await;
    start_cloud(&env).await;
    plan(&env).await;
    assert_eq!(jobs(&env).await.len(), 3);
    work(&env).await;
    assert!(jobs(&env).await.is_empty());
    assert_eq!(sent_pictures(&fake).await.len(), 1);
    for name in ["klein.jpg", "kaputt.jpg"] {
        let h = hash(&env, &home, name).await;
        assert!(sources_of(&env, &h).await.is_empty());
        assert_eq!(vision_of(&env, &h).await, None);
    }

    // Another text model: the descriptions are embedded again, the pictures not looked at again.
    restart(&mut env, |c| {
        c.ai.cloud.as_mut().unwrap().embed_model = "qwen3-embedding-0.6b".into();
    });
    fake.clear().await;
    pipeline::sweep(&env.state).await.unwrap();
    work(&env).await;
    let seen = fake.seen().await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].path, "/v1/embeddings");
    assert_eq!(seen[0].body["model"], "qwen3-embedding-0.6b");
    let strand = hash(&env, &home, "IMG_0001.jpg").await;
    assert!(sources_of(&env, &strand).await.contains(&row(
        "cloud",
        "qwen3-embedding-0.6b",
        "description"
    )));
    env.finish().await;
}

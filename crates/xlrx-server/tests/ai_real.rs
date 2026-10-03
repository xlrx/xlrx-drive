//! AI search with the real home-network service (`embed-local` with multilingual-e5-small and
//! multilingual CLIP): German queries find texts by meaning and photos by what they show, in a
//! "Nur lokal" folder, with the distance limits the server uses by default.
//!
//! Runs only with `XLRX_TEST_EMBED_LOCAL=http://127.0.0.1:8090/v1` (the service running, see
//! `crates/embed-local`) and `XLRX_TEST_PHOTOS` (dog_beach.jpg, snow_mountain.jpg, car.jpg,
//! cat.jpg).

mod common;

use std::time::Duration;

use common::ai::*;
use common::files::*;
use common::*;
use serde_json::Value;
use xlrx_server::AppState;
use xlrx_server::ai::search::{default_max_distance, default_spread};
use xlrx_server::config::LocalAi;
use xlrx_server::files::data_class::Class;
use xlrx_server::search;

async fn find(c: &mut Client, q: &str) -> Value {
    let q: String = url::form_urlencoded::byte_serialize(q.as_bytes()).collect();
    c.get(&format!("/api/search?q={q}")).await.ok().clone()
}

#[tokio::test]
async fn echte_modelle_im_heimnetz() {
    let (Ok(url), Ok(photos)) = (
        std::env::var("XLRX_TEST_EMBED_LOCAL"),
        std::env::var("XLRX_TEST_PHOTOS"),
    ) else {
        eprintln!("XLRX_TEST_EMBED_LOCAL/XLRX_TEST_PHOTOS nicht gesetzt – übersprungen");
        return;
    };
    let Some(mut env) = Env::with_data().await else {
        return;
    };
    let mut cfg = env.state.cfg.clone();
    cfg.default_data_class = Class::Local;
    let e5 = "multilingual-e5-small";
    let clip = "clip-ViT-B-32-multilingual-v1";
    cfg.ai.local = Some(LocalAi {
        url: url.parse().unwrap(),
        embed_model: e5.into(),
        embed_dim: 384,
        clip_model: Some(clip.into()),
        clip_dim: 512,
        max_distance: default_max_distance(e5),
        clip_max_distance: default_max_distance(clip),
        spread: default_spread(e5),
        clip_spread: default_spread(clip),
    });
    env.state = AppState::new(env.db.pool.clone(), cfg).unwrap();
    env.app = xlrx_server::router(env.state.clone());
    search::start(&env.state).await.unwrap();
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Privat/Wartung.txt", "Rechnung der Firma Müller über die Wartung der Heizung im November. Betrag 238 Euro."),
            ("Privat/Packliste.txt", "Packliste für den Urlaub am Strand: Sonnencreme, Handtuch, Badehose und ein Buch."),
            ("Privat/Labor.txt", "Befund der Blutwerte vom Hausarzt: Cholesterin leicht erhöht, Kontrolle in drei Monaten."),
            ("Privat/Mietvertrag.txt", "Mietvertrag für die Wohnung in der Lindenstraße, Kündigungsfrist drei Monate."),
            ("Privat/Kuchen.txt", "Rezept für Apfelkuchen: 500 g Mehl, 200 g Zucker, vier Äpfel, Zimt."),
        ],
    )
    .await;
    let photos = std::path::PathBuf::from(photos);
    for (n, to) in [
        ("dog_beach", "IMG_1001.jpg"),
        ("snow_mountain", "IMG_1002.jpg"),
        ("car", "IMG_1003.jpg"),
        ("cat", "IMG_1004.jpg"),
    ] {
        write(
            &dir.join("Privat").join(to),
            &std::fs::read(photos.join(format!("{n}.jpg"))).unwrap(),
        );
    }
    xlrx_server::files::roots::scan(&env.state, &home)
        .await
        .unwrap();
    plan(&env).await;
    work(&env).await;
    let (texts, pictures): (i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE space = 'local'), count(*) FILTER (WHERE space = 'clip') FROM ai_vectors",
    )
    .fetch_one(&env.db.pool)
    .await
    .unwrap();
    assert_eq!((texts, pictures), (5, 4));
    for _ in 0..100 {
        if !find(&mut klaus, "Packliste").await["hits"]
            .as_array()
            .unwrap()
            .is_empty()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    for (q, want) in [
        ("Ferien am Meer", "Packliste.txt"),
        ("Laborwerte Arzt", "Labor.txt"),
        ("Wohnung kündigen", "Mietvertrag.txt"),
        ("Kosten Heizungswartung", "Wartung.txt"),
        ("Hund am Strand", "IMG_1001.jpg"),
        ("Berge im Schnee", "IMG_1002.jpg"),
        ("rotes Auto", "IMG_1003.jpg"),
        ("schlafende Katze", "IMG_1004.jpg"),
    ] {
        let r = find(&mut klaus, q).await;
        let names: Vec<&str> = r["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["name"].as_str().unwrap())
            .collect();
        eprintln!("„{q}“: {names:?}");
        // Among the first two (a file with one of the words may come first, as "Strand" in the
        // packing list), and the first of its kind.
        assert!(names.iter().take(2).any(|n| *n == want), "{q}: {names:?}");
        let picture = want.ends_with(".jpg");
        let first_of_kind = names.iter().find(|n| n.ends_with(".jpg") == picture);
        assert_eq!(first_of_kind, Some(&want), "{q}");
    }
    // Something no file is about finds little or nothing.
    let r = find(&mut klaus, "Quantenphysik Vorlesung").await;
    assert!(r["hits"].as_array().unwrap().len() <= 2, "{r}");
    env.finish().await;
}

//! AI search, M4.1: texts become vectors – in the cloud for "Cloud erlaubt", in the home network
//! for "Nur lokal"; nothing from "Nur lokal" reaches the cloud, results are revoked when a folder
//! becomes "Nur lokal", the cloud runs only when started and within its budget.

mod common;

use common::ai::*;
use common::ai::{self as fake, FakeAi};
use common::files::*;
use serde_json::json;
use xlrx_server::ai::{self, Side, Space, pipeline, vectors};
use xlrx_server::files::data_class::Class;
use xlrx_server::files::roots;
use xlrx_server::search::text;

const HEIZUNG: &str = "Rechnung der Firma Müller über die Wartung der Heizung im November. \
                       Bitte überweisen Sie den Betrag innerhalb von 14 Tagen.";
const BEFUND: &str = "Befund der Blutwerte vom Hausarzt: Cholesterin leicht erhöht, \
                      Kontrolle in drei Monaten empfohlen.";
const URLAUB: &str = "Packliste für den Urlaub am Strand: Sonnencreme, Handtuch, Badehose, \
                      Sonnenbrille und ein gutes Buch.";

#[tokio::test]
async fn texte_in_der_cloud_und_im_heimnetz() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Arbeit/Heizung.txt", HEIZUNG),
            ("Arbeit/Urlaub.txt", URLAUB),
            ("Arbeit/Zahlen.txt", "1 2 3 4"),
            ("Gesundheit/Befund.txt", BEFUND),
        ],
    )
    .await;
    let arbeit = node(&env, &home, "Arbeit").await.id;
    set_class(&mut klaus, arbeit, "cloud").await;
    start_cloud(&env).await;
    let heizung = hash(&env, &home, "Arbeit/Heizung.txt").await;
    let urlaub = hash(&env, &home, "Arbeit/Urlaub.txt").await;
    let zahlen = hash(&env, &home, "Arbeit/Zahlen.txt").await;
    let befund = hash(&env, &home, "Gesundheit/Befund.txt").await;

    plan(&env).await;
    let mut want = vec![
        (
            "ai_cloud".to_string(),
            key(&heizung),
            "queued".to_string(),
            0,
        ),
        ("ai_cloud".into(), key(&urlaub), "queued".into(), 0),
        ("ai_cloud".into(), key(&zahlen), "queued".into(), 0),
        ("ai_local".into(), key(&befund), "queued".into(), 0),
    ];
    want.sort();
    assert_eq!(jobs(&env).await, want);
    work(&env).await;
    assert!(jobs(&env).await.is_empty());

    assert_eq!(vectors_of(&env, &heizung).await, cloud_vec(1));
    assert_eq!(vectors_of(&env, &urlaub).await, cloud_vec(1));
    assert_eq!(vectors_of(&env, &befund).await, local_vec(1));
    // Too little text: nothing sent, but done.
    assert!(vectors_of(&env, &zahlen).await.is_empty());
    let done: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_done WHERE content_hash = $1")
        .bind(&zahlen)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(done, 1);

    // What went where: the cloud with its key, only texts from "Cloud erlaubt", never names.
    let seen = fake.seen().await;
    let (cloud, local): (Vec<_>, Vec<_>) = seen.iter().partition(|s| s.auth.is_some());
    assert!(
        cloud
            .iter()
            .all(|s| s.auth.as_deref() == Some("Bearer test-schluessel"))
    );
    assert!(cloud.iter().all(|s| s.body["dimensions"] == 256));
    assert!(
        cloud
            .iter()
            .all(|s| s.body["model"] == "qwen3-embedding-8b")
    );
    let to_cloud: String = cloud.iter().flat_map(|s| s.inputs()).collect();
    assert!(to_cloud.contains("Wartung der Heizung") && to_cloud.contains("Sonnencreme"));
    assert!(!to_cloud.contains("Blutwerte"));
    assert_eq!(local.len(), 1);
    assert_eq!(local[0].body["model"], "multilingual-e5-small");
    assert!(local[0].body.get("dimensions").is_none());
    assert_eq!(
        local[0].inputs(),
        vec![format!(
            "passage: {}",
            BEFUND.split_whitespace().collect::<Vec<_>>().join(" ")
        )]
    );
    let all = fake.sent_text().await;
    for name in ["Heizung.txt", "Befund.txt", "Arbeit", "Gesundheit", "klaus"] {
        assert!(!all.contains(name), "{name} gesendet");
    }

    // Found by meaning.
    assert_eq!(
        nearest(&env, Space::Cloud, "Rechnung Heizung").await[0],
        heizung
    );
    assert_eq!(
        nearest(&env, Space::Cloud, "Strand Urlaub").await[0],
        urlaub
    );
    assert_eq!(
        nearest(&env, Space::Local, "Blutwerte Arzt").await,
        vec![befund.clone()]
    );

    // Costs are counted.
    let (calls, tokens, cost): (i64, i64, f64) =
        sqlx::query_as("SELECT calls, tokens_in, cost FROM ai_usage WHERE kind = 'embed'")
            .fetch_one(&env.db.pool)
            .await
            .unwrap();
    assert_eq!(calls, 2);
    assert!(tokens > 20);
    assert!((cost - tokens as f64 / 1e6 * 0.10).abs() < 1e-12);
    assert!(ai::month_spent(&env.state).await.unwrap() > 0.0);

    // Once per content: a copy, a rename, the next round cost nothing.
    fake.clear().await;
    files(
        &env,
        &home,
        &dir,
        &[("Arbeit/Kopie/Heizung 2.txt", HEIZUNG)],
    )
    .await;
    let mut c = klaus;
    let id = node(&env, &home, "Arbeit/Urlaub.txt").await.id;
    c.send(
        "PATCH",
        &format!("/api/nodes/{id}"),
        Some(json!({ "name": "Ferien.txt" })),
    )
    .await
    .ok();
    plan(&env).await;
    work(&env).await;
    assert!(fake.seen().await.is_empty());
    assert!(jobs(&env).await.is_empty());

    // A better text for the same content (read again, e.g. by OCR) is embedded anew.
    let better = format!("{HEIZUNG} Austausch des Brenners und Kesselreinigung.");
    text::store(
        &env.db.pool,
        &heizung.clone().try_into().unwrap(),
        "ocr",
        &better,
    )
    .await
    .unwrap();
    plan(&env).await;
    work(&env).await;
    let seen = fake.seen().await;
    assert_eq!(seen.len(), 1);
    assert!(seen[0].inputs()[0].contains("Kesselreinigung"));
    assert_eq!(vectors_of(&env, &heizung).await, cloud_vec(1));
    assert_eq!(
        nearest(&env, Space::Cloud, "Kesselreinigung Brenner").await[0],
        heizung
    );
    env.finish().await;
}

#[tokio::test]
async fn cloud_erst_nach_start_im_probelauf_und_im_budget() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let (_, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Heizung.txt", HEIZUNG),
            ("Urlaub.txt", URLAUB),
            ("Notiz.txt", BEFUND),
        ],
    )
    .await;
    plan(&env).await;
    assert_eq!(jobs(&env).await.len(), 3);

    // Not started: nothing is sent, the jobs wait without using up attempts.
    assert_eq!(
        ai::cloud_paused(&env.state, 0.0).await.unwrap(),
        Some("Die Cloud-Analyse ist nicht gestartet")
    );
    work(&env).await;
    assert!(fake.seen().await.is_empty());
    let j = jobs(&env).await;
    assert_eq!(j.len(), 3);
    assert!(
        j.iter()
            .all(|(_, _, state, attempts)| state == "queued" && *attempts == 0),
        "{j:?}"
    );

    // A trial run of one content: one is done, then it stops again.
    sqlx::query("UPDATE ai_state SET trial_left = 1")
        .execute(&env.db.pool)
        .await
        .unwrap();
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 1);
    assert_eq!(jobs(&env).await.len(), 2);
    let left: Option<i32> = sqlx::query_scalar("SELECT trial_left FROM ai_state")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(left, Some(0));
    assert!(ai::cloud_paused(&env.state, 0.0).await.unwrap().is_some());

    // Started, but the month's budget is spent.
    let spent = ai::month_spent(&env.state).await.unwrap();
    sqlx::query("UPDATE ai_state SET cloud_on = true, budget = $1")
        .bind(spent)
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(
        ai::cloud_paused(&env.state, 0.0).await.unwrap(),
        Some("Das Monatsbudget ist ausgeschöpft")
    );
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 1);
    // A call that would go over the budget waits, too.
    sqlx::query("UPDATE ai_state SET budget = $1")
        .bind(spent + 1e-9)
        .execute(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(ai::cloud_paused(&env.state, 0.0).await.unwrap(), None);
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 1);

    // The configured budget (20 €) applies again.
    sqlx::query("UPDATE ai_state SET budget = NULL")
        .execute(&env.db.pool)
        .await
        .unwrap();
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 3);
    assert!(jobs(&env).await.is_empty());
    env.finish().await;
}

#[tokio::test]
async fn nur_lokal_loescht_was_die_cloud_gemacht_hat() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Arbeit/Heizung.txt", HEIZUNG),
            ("Arbeit/Reise/Urlaub.txt", URLAUB),
        ],
    )
    .await;
    std::fs::create_dir_all(dir.join("Privat")).unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let arbeit = node(&env, &home, "Arbeit").await.id;
    set_class(&mut klaus, arbeit, "cloud").await;
    start_cloud(&env).await;
    plan(&env).await;
    work(&env).await;
    let heizung = hash(&env, &home, "Arbeit/Heizung.txt").await;
    let urlaub = hash(&env, &home, "Arbeit/Reise/Urlaub.txt").await;
    assert_eq!(vectors_of(&env, &heizung).await, cloud_vec(1));

    // The folder becomes "Nur lokal": its cloud results go at once, logged, and the home network
    // makes them again.
    set_class(&mut klaus, arbeit, "local").await;
    assert!(vectors_of(&env, &heizung).await.is_empty());
    let cloud_done: i64 = sqlx::query_scalar("SELECT count(*) FROM ai_done WHERE space = 'cloud'")
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(cloud_done, 0);
    let logged: serde_json::Value = sqlx::query_scalar(
        "SELECT details FROM audit_log WHERE action = 'ai_revoked' ORDER BY id DESC LIMIT 1",
    )
    .fetch_one(&env.db.pool)
    .await
    .unwrap();
    assert_eq!(logged["contents"], 2);
    let mut want = vec![
        (
            "ai_local".to_string(),
            key(&heizung),
            "queued".to_string(),
            0,
        ),
        ("ai_local".into(), key(&urlaub), "queued".into(), 0),
    ];
    want.sort();
    assert_eq!(jobs(&env).await, want);
    fake.clear().await;
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, local_vec(1));
    assert!(fake.seen().await.iter().all(|s| s.auth.is_none()));

    // Allowed again: the cloud takes over, the local vectors go.
    set_class(&mut klaus, arbeit, "cloud").await;
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, cloud_vec(1));
    assert_eq!(vectors_of(&env, &urlaub).await, cloud_vec(1));

    // A copy into a "Nur lokal" folder is enough.
    files(&env, &home, &dir, &[("Privat/Heizung Kopie.txt", HEIZUNG)]).await;
    plan(&env).await;
    assert!(vectors_of(&env, &heizung).await.is_empty());
    assert_eq!(vectors_of(&env, &urlaub).await, cloud_vec(1));
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, local_vec(1));

    // So is moving a folder holding it there (one without a class of its own: a folder set to
    // "Cloud erlaubt" keeps that wherever it goes).
    std::fs::rename(dir.join("Arbeit/Reise"), dir.join("Privat/Reise")).unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    plan(&env).await;
    assert!(vectors_of(&env, &urlaub).await.is_empty());
    work(&env).await;
    assert_eq!(vectors_of(&env, &urlaub).await, local_vec(1));
    env.finish().await;
}

#[tokio::test]
async fn nur_lokal_waehrend_der_anfrage_verwirft_das_ergebnis() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Local).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(&env, &home, &dir, &[("Arbeit/Heizung.txt", HEIZUNG)]).await;
    let arbeit = node(&env, &home, "Arbeit").await.id;
    set_class(&mut klaus, arbeit, "cloud").await;
    start_cloud(&env).await;
    plan(&env).await;
    let heizung = hash(&env, &home, "Arbeit/Heizung.txt").await;

    // While the cloud works, the folder becomes "Nur lokal" (before anything is revoked).
    fake.before_answer(
        env.db.pool.clone(),
        &format!("UPDATE data_classes SET class = 'local' WHERE node_id = {arbeit}"),
    )
    .await;
    assert!(pipeline::work_one(&env.state, Side::Cloud).await.unwrap());
    assert_eq!(fake.seen().await.len(), 1);
    assert!(vectors_of(&env, &heizung).await.is_empty());
    assert_eq!(
        jobs(&env).await,
        vec![(
            "ai_local".to_string(),
            key(&heizung),
            "queued".to_string(),
            0
        )]
    );
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, local_vec(1));
    env.finish().await;
}

#[tokio::test]
async fn pruefung_widerruft_raeumt_auf_und_holt_nach() {
    let fake = FakeAi::start().await;
    let Some(mut env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let (_, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[("Heizung.txt", HEIZUNG), ("Urlaub.txt", URLAUB)],
    )
    .await;
    start_cloud(&env).await;
    plan(&env).await;
    work(&env).await;
    let heizung = hash(&env, &home, "Heizung.txt").await;
    let urlaub = hash(&env, &home, "Urlaub.txt").await;
    assert_eq!(
        pipeline::sweep(&env.state).await.unwrap(),
        Default::default()
    );

    // A file gone for good: its results go.
    std::fs::remove_file(dir.join("Urlaub.txt")).unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let s = pipeline::sweep(&env.state).await.unwrap();
    assert_eq!((s.removed, s.revoked), (1, 0));
    assert!(vectors_of(&env, &urlaub).await.is_empty());

    // The default became "Nur lokal" (a setting, no folder changed): the check revokes.
    restart(&mut env, |c| c.default_data_class = Class::Local);
    let s = pipeline::sweep(&env.state).await.unwrap();
    assert_eq!(s.revoked, 1);
    assert!(vectors_of(&env, &heizung).await.is_empty());
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, local_vec(1));

    // Work lost (e.g. jobs deleted) is found again.
    sqlx::query("DELETE FROM ai_done")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let s = pipeline::sweep(&env.state).await.unwrap();
    assert_eq!(s.queued, 1);
    env.finish().await;
}

#[tokio::test]
async fn fehler_des_anbieters() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let (_, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(&env, &home, &dir, &[("Heizung.txt", HEIZUNG)]).await;
    start_cloud(&env).await;
    plan(&env).await;
    let heizung = hash(&env, &home, "Heizung.txt").await;

    // Overloaded: try again later, the error is shown.
    fake.fail_with(Some(503)).await;
    assert!(pipeline::work_one(&env.state, Side::Cloud).await.unwrap());
    assert_eq!(
        jobs(&env).await,
        vec![(
            "ai_cloud".to_string(),
            key(&heizung),
            "queued".to_string(),
            1
        )]
    );
    assert!(env.state.ai.error(Side::Cloud).unwrap().contains("503"));
    // Not due yet.
    assert!(!pipeline::work_one(&env.state, Side::Cloud).await.unwrap());

    // The provider refuses this content: given up at once.
    sqlx::query("UPDATE jobs SET run_after = now()")
        .execute(&env.db.pool)
        .await
        .unwrap();
    fake.fail_with(Some(400)).await;
    assert!(pipeline::work_one(&env.state, Side::Cloud).await.unwrap());
    assert_eq!(jobs(&env).await[0].2, "failed");

    // Recovered: the error is gone after the next success.
    fake.fail_with(None).await;
    sqlx::query("UPDATE jobs SET state = 'queued', run_after = now()")
        .execute(&env.db.pool)
        .await
        .unwrap();
    work(&env).await;
    assert_eq!(vectors_of(&env, &heizung).await, cloud_vec(1));
    assert_eq!(env.state.ai.error(Side::Cloud), None);
    env.finish().await;
}

#[tokio::test]
async fn vektorindizes_werden_genutzt() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let db = &env.db.pool;
    for (space, model, dim) in [
        (Space::Cloud, "qwen3-embedding-8b", fake::CLOUD_DIM),
        (
            Space::Local,
            "multilingual-e5-small",
            fake::LOCAL_DIM as u32,
        ),
    ] {
        vectors::ensure_index(db, space, model, dim).await.unwrap();
        // Twice is fine; an interrupted build (invalid index) is made again.
        vectors::ensure_index(db, space, model, dim).await.unwrap();
        let name = vectors::index_name(space, model, dim);
        sqlx::query(
            "UPDATE pg_index SET indisvalid = false
              WHERE indexrelid = (SELECT oid FROM pg_class WHERE relname = $1)",
        )
        .bind(&name)
        .execute(db)
        .await
        .unwrap();
        vectors::ensure_index(db, space, model, dim).await.unwrap();
        let valid: bool = sqlx::query_scalar(
            "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
              WHERE c.relname = $1",
        )
        .bind(&name)
        .fetch_one(db)
        .await
        .unwrap();
        assert!(valid);

        // Some vectors of this and of another model.
        for i in 0..200 {
            let mut v = fake::embed(
                &format!("wort{i} text{} thema{}", i % 7, i % 13),
                dim as usize,
            );
            vectors::normalize(&mut v);
            for m in [model, "anderes-modell"] {
                sqlx::query(
                    "INSERT INTO ai_vectors (content_hash, space, model, source, vec)
                     VALUES ($1, $2, $3, 'text', $4::text::halfvec)",
                )
                .bind(vec![i as u8; 32])
                .bind(space.as_str())
                .bind(m)
                .bind(vectors::literal(&v))
                .execute(db)
                .await
                .unwrap();
            }
        }
        sqlx::query("ANALYZE ai_vectors").execute(db).await.unwrap();
        let (sql, _) = vectors::nearest_sql(space, model, dim, 10);
        let mut v = fake::embed("wort5 text5", dim as usize);
        vectors::normalize(&mut v);
        let mut tx = db.begin().await.unwrap();
        // Few rows: other plans look cheaper; the index must be usable all the same.
        for off in ["enable_seqscan", "enable_bitmapscan"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("SET LOCAL {off} = off")))
                .execute(&mut *tx)
                .await
                .unwrap();
        }
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("EXPLAIN {sql}")))
            .bind(vectors::literal(&v))
            .fetch_all(&mut *tx)
            .await
            .unwrap();
        tx.rollback().await.unwrap();
        let plan = plan.join("\n");
        assert!(plan.contains(&name), "{space:?}: {plan}");

        let near = vectors::nearest(db, space, model, dim, &v, 10)
            .await
            .unwrap();
        assert_eq!(near.len(), 10);
        assert_eq!(near[0].content_hash, vec![5u8; 32]);
        assert!(near.windows(2).all(|w| w[0].dist <= w[1].dist));
    }
    env.finish().await;
}

#[tokio::test]
async fn verwaltung_starten_probelauf_budget_und_kosten() {
    let fake = FakeAi::start().await;
    let Some(env) = env_with(&fake, Class::Cloud).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let home = root(&env, root_id).await;
    files(
        &env,
        &home,
        &dir,
        &[
            ("Heizung.txt", HEIZUNG),
            ("Urlaub.txt", URLAUB),
            ("Notiz.txt", BEFUND),
        ],
    )
    .await;
    plan(&env).await;

    // Only administrators.
    assert_eq!(klaus.get("/api/admin/ai").await.status, 403);
    sqlx::query("UPDATE users SET is_admin = true WHERE username = 'klaus'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let st = klaus.get("/api/admin/ai").await.ok().clone();
    assert_eq!(st["cloud"]["provider"], "127.0.0.1");
    assert_eq!(st["cloud"]["embed_model"], "qwen3-embedding-8b");
    assert_eq!(st["cloud"]["on"], false);
    assert_eq!(
        st["cloud"]["paused"],
        "Die Cloud-Analyse ist nicht gestartet"
    );
    assert_eq!(st["cloud"]["budget"], 20.0);
    assert_eq!(st["local"]["clip_model"], fake::CLIP_MODEL);
    assert_eq!(st["progress"]["queued_cloud"], 3);
    assert!(st["estimate"].is_null());

    // Costs money: only with a fresh second factor.
    sqlx::query("UPDATE sessions SET step_up_at = now() - interval '1 day'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = klaus
        .post("/api/admin/ai", json!({"action": "trial", "trial": 2}))
        .await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".into())
    );
    sqlx::query("UPDATE sessions SET step_up_at = now()")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let st = klaus
        .post("/api/admin/ai", json!({"action": "trial", "trial": 2}))
        .await
        .ok()
        .clone();
    assert_eq!(st["cloud"]["trial_left"], 2);
    assert!(st["cloud"]["paused"].is_null());
    work(&env).await;
    let st = klaus.get("/api/admin/ai").await.ok().clone();
    assert_eq!(st["cloud"]["trial_left"], 0);
    assert_eq!(st["progress"]["cloud_texts"], 2);
    assert_eq!(st["progress"]["queued_cloud"], 1);
    assert_eq!(st["progress"]["last_hour_cloud"], 2);
    // What the trial run cost per content, and what the rest will cost.
    let spent = st["cloud"]["spent_month"].as_f64().unwrap();
    assert!(spent > 0.0);
    let per_text = st["estimate"]["per_text"].as_f64().unwrap();
    assert!((per_text - spent / 2.0).abs() < 1e-12);
    assert!((st["estimate"]["remaining"].as_f64().unwrap() - per_text).abs() < 1e-12);
    assert_eq!(st["usage"][0]["kind"], "embed");
    assert_eq!(st["usage"][0]["calls"], 2);

    // A budget below what was spent stops it; the configured one comes back with null.
    let st = klaus
        .send(
            "PUT",
            "/api/admin/ai/budget",
            Some(json!({"budget": spent / 2.0})),
        )
        .await
        .ok()
        .clone();
    assert_eq!(st["cloud"]["budget_set"], true);
    let st = klaus
        .post("/api/admin/ai", json!({"action": "start"}))
        .await
        .ok()
        .clone();
    assert_eq!(st["cloud"]["on"], true);
    assert_eq!(st["cloud"]["paused"], "Das Monatsbudget ist ausgeschöpft");
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 2);
    let r = klaus
        .send("PUT", "/api/admin/ai/budget", Some(json!({"budget": -1})))
        .await;
    assert_eq!(r.status, 400);
    let st = klaus
        .send("PUT", "/api/admin/ai/budget", Some(json!({"budget": null})))
        .await
        .ok()
        .clone();
    assert_eq!(
        (
            st["cloud"]["budget"].as_f64(),
            st["cloud"]["budget_set"].as_bool()
        ),
        (Some(20.0), Some(false))
    );
    work(&env).await;
    assert_eq!(fake.seen().await.len(), 3);
    let st = klaus
        .post("/api/admin/ai", json!({"action": "stop"}))
        .await
        .ok()
        .clone();
    assert_eq!(st["cloud"]["on"], false);
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_log WHERE action LIKE 'ai_%' ORDER BY id")
            .fetch_all(&env.db.pool)
            .await
            .unwrap();
    assert_eq!(
        actions,
        [
            "ai_trial_started",
            "ai_budget_set",
            "ai_started",
            "ai_budget_set",
            "ai_stopped"
        ]
    );
    env.finish().await;
}

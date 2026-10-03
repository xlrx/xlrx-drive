//! Outside cache (PLAN 15.2) against a real S3 implementation: what gets mirrored, who is sent
//! there, and that "Nur lokal" contents are never in the bucket.

mod common;

use std::collections::HashMap;

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use common::files::*;
use common::s3::FakeS3;
use common::*;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::rt::TokioExecutor;
use serde_json::json;
use tower::ServiceExt;
use xlrx_server::files::db::{self, RootRow};
use xlrx_server::files::{mirror, roots};

const OUTSIDE: &str = "203.0.113.7";
const INSIDE: &str = "192.168.1.20";

/// Distinct contents of some size.
fn data(seed: u8, len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u32 * 31 + seed as u32 * 7919) as u8 ^ seed)
        .collect()
}

struct Got {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Got {
    fn location(&self) -> &str {
        self.headers
            .get(header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

/// A GET as from this address; with a session if given.
async fn get(env: &Env, path: &str, ip: &str, session: Option<&Client>) -> Got {
    let mut r = Request::builder().uri(path).header("x-forwarded-for", ip);
    if let Some(c) = session.and_then(|c| c.cookie.clone()) {
        r = r.header(header::COOKIE, format!("xlrx_session={c}"));
    }
    let res = env
        .app
        .clone()
        .oneshot(r.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    Got {
        status,
        headers,
        body,
    }
}

/// Follows a presigned URL to the bucket.
async fn fetch(url: &str) -> (u16, Vec<u8>) {
    let client = HttpClient::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
    let res = client
        .request(hyper::Request::get(url).body(Empty::new()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    (
        status,
        res.into_body().collect().await.unwrap().to_bytes().to_vec(),
    )
}

async fn round(env: &Env) -> mirror::Round {
    mirror::round(&env.state, &mut HashMap::new())
        .await
        .unwrap()
}

async fn root(env: &Env, id: i64) -> RootRow {
    db::root_by_id(&env.db.pool, id).await.unwrap().unwrap()
}

async fn link(c: &mut Client, id: i64, kind: &str) -> (i64, String) {
    let r = c
        .post(&format!("/api/nodes/{id}/links"), json!({ "kind": kind }))
        .await;
    assert_eq!(r.status, 201, "{}", r.body);
    let token = r.body["url"]
        .as_str()
        .unwrap()
        .rsplit('/')
        .next()
        .unwrap()
        .to_owned();
    (r.body["id"].as_i64().unwrap(), token)
}

async fn objects(env: &Env) -> Vec<(String, String)> {
    sqlx::query_as("SELECT state, reason FROM s3_objects ORDER BY created_at")
        .fetch_all(&env.db.pool)
        .await
        .unwrap()
}

async fn hash_of(env: &Env, node: i64) -> Vec<u8> {
    sqlx::query_scalar("SELECT content_hash FROM nodes WHERE id = $1")
        .bind(node)
        .fetch_one(&env.db.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn link_dateien_von_aussen_aus_dem_bucket() {
    let fake = FakeS3::start().await;
    let Some(env) = Env::with_s3(&fake).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let film = data(1, 200_000);
    write(&dir.join("Haus/Film.mp4"), &film);
    write(&dir.join("Haus/Foto.jpg"), &data(2, 50_000));
    write(&dir.join("Haus/klein.txt"), b"klein");
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let f = node(&env, &home, "Haus/Film.mp4").await.id;
    let foto = node(&env, &home, "Haus/Foto.jpg").await.id;
    let klein = node(&env, &home, "Haus/klein.txt").await.id;

    let (film_link, t) = link(&mut klaus, f, "download").await;
    link(&mut klaus, klein, "download").await;
    assert_eq!(
        round(&env).await.uploaded,
        1,
        "die kleine Datei kommt vom NAS"
    );
    assert_eq!(objects(&env).await, [("ready".into(), "link".into())]);
    // The bucket sees an anonymous name: neither the file name nor its content hash.
    let keys = fake.keys();
    let hash: String = hash_of(&env, f)
        .await
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].starts_with("x/") && !keys[0].contains(&hash) && !keys[0].contains("Film"));

    // From outside: sent to the bucket, which serves exactly the file.
    let path = format!("/api/public/{t}/nodes/{f}/content");
    let g = get(&env, &path, OUTSIDE, None).await;
    assert_eq!(g.status, StatusCode::TEMPORARY_REDIRECT);
    let url = g.location().to_owned();
    assert!(
        url.starts_with(fake.cfg.endpoint.as_str().trim_end_matches('/'))
            && url.contains("X-Amz-Signature=")
    );
    assert!(
        url.contains("response-content-disposition=attachment"),
        "{url}"
    );
    let (status, body) = fetch(&url).await;
    assert!(status == 200 && body == film, "{status}");
    // Counted like any download.
    let downloads: i32 = sqlx::query_scalar("SELECT downloads FROM links WHERE id = $1")
        .bind(film_link)
        .fetch_one(&env.db.pool)
        .await
        .unwrap();
    assert_eq!(downloads, 1);
    // Watching the video too.
    let g = get(&env, &format!("{path}?inline=true"), OUTSIDE, None).await;
    assert_eq!(g.status, StatusCode::TEMPORARY_REDIRECT);
    assert!(g.location().contains("response-content-type=video%2Fmp4"));
    // In the home network, or from an unknown address: always the NAS.
    for ip in [INSIDE, "fd00::17", "2001:db8:1:2::9", "unbekannt"] {
        let g = get(&env, &path, ip, None).await;
        assert!(
            g.status == StatusCode::OK && g.body == film,
            "{ip}: {}",
            g.status
        );
    }
    // Signed in and away: the same.
    let g = get(
        &env,
        &format!("/api/nodes/{f}/content"),
        OUTSIDE,
        Some(&klaus),
    )
    .await;
    assert_eq!(g.status, StatusCode::TEMPORARY_REDIRECT);
    let g = get(
        &env,
        &format!("/api/nodes/{f}/content"),
        INSIDE,
        Some(&klaus),
    )
    .await;
    assert_eq!(g.status, StatusCode::OK);
    // Not mirrored: from the NAS; pictures shown in the browser always from the NAS.
    let g = get(
        &env,
        &format!("/api/nodes/{klein}/content"),
        OUTSIDE,
        Some(&klaus),
    )
    .await;
    assert_eq!(
        (g.status, g.body.as_slice()),
        (StatusCode::OK, &b"klein"[..])
    );
    let g = get(
        &env,
        &format!("/api/nodes/{foto}/content?inline=true"),
        OUTSIDE,
        Some(&klaus),
    )
    .await;
    assert_eq!(g.status, StatusCode::OK);

    // The link ends: the object goes, and every URL handed out with it.
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/links/{film_link}"), None)
            .await
            .status,
        204
    );
    round(&env).await;
    assert!(!fake.keys().contains(&keys[0]));
    assert_ne!(fetch(&url).await.0, 200);
    // Fetched from outside twice (through the link and by klaus away), it is mirrored again as
    // popular – under a new name, so old URLs stay dead.
    assert_eq!(objects(&env).await, [("ready".into(), "popular".into())]);
    assert_eq!(fake.keys().len(), 1);
    assert_ne!(fake.keys(), keys);
    env.finish().await;
}

#[tokio::test]
async fn nur_lokal_nie_im_bucket() {
    let fake = FakeS3::start().await;
    let Some(env) = Env::with_s3(&fake).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    let befund = data(3, 20_000);
    write(&dir.join("Gesundheit/Befund.pdf"), &befund);
    write(&dir.join("Haus/Kopie Befund.pdf"), &befund);
    write(&dir.join("Haus/Video.mp4"), &data(4, 30_000));
    write(&dir.join("Gesundheit/Alt.pdf"), &data(5, 20_000));
    write(&dir.join("Haus/Wie Alt.pdf"), &data(5, 20_000));
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let gesundheit = node(&env, &home, "Gesundheit").await.id;
    let haus = node(&env, &home, "Haus").await.id;
    let kopie = node(&env, &home, "Haus/Kopie Befund.pdf").await.id;
    let video = node(&env, &home, "Haus/Video.mp4").await.id;
    let alt = node(&env, &home, "Gesundheit/Alt.pdf").await;
    let wie_alt = node(&env, &home, "Haus/Wie Alt.pdf").await.id;
    let r = klaus
        .send(
            "PUT",
            &format!("/api/nodes/{gesundheit}/data-class"),
            Some(json!({"class": "local"})),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    // "Alt.pdf" gets a new content; its old one lives on as a version – of a "Nur lokal" file.
    let r = klaus
        .send_bytes(
            "PUT",
            &format!("/api/nodes/{}/content?base_rev={}", alt.id, alt.rev),
            data(6, 20_000),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);

    // A copy of a "Nur lokal" content in a "Cloud erlaubt" folder: never mirrored, also not as an
    // old version.
    link(&mut klaus, kopie, "download").await;
    link(&mut klaus, wie_alt, "download").await;
    let (_, t) = link(&mut klaus, video, "download").await;
    assert_eq!(round(&env).await.uploaded, 1);
    assert_eq!(
        fake.uploads(),
        1,
        "nichts Verbotenes hochgeladen, auch nicht kurz"
    );
    assert_eq!(fake.keys().len(), 1);
    let g = get(
        &env,
        &format!("/api/nodes/{kopie}/content"),
        OUTSIDE,
        Some(&klaus),
    )
    .await;
    assert_eq!(g.status, StatusCode::OK);

    // The video is out – then a copy of it lands in "Gesundheit": not handed out any more,
    // gone from the bucket with the next round.
    std::fs::copy(dir.join("Haus/Video.mp4"), dir.join("Gesundheit/Video.mp4")).unwrap();
    roots::scan(&env.state, &home).await.unwrap();
    let path = format!("/api/public/{t}/nodes/{video}/content");
    let g = get(&env, &path, OUTSIDE, None).await;
    assert_eq!(g.status, StatusCode::OK, "nicht mehr umgeleitet");
    assert_eq!(objects(&env).await, [("deleting".into(), "link".into())]);
    round(&env).await;
    assert!(fake.keys().is_empty());
    // It stays out while the copy is there.
    round(&env).await;
    assert!(fake.keys().is_empty());

    // In the trash of "Gesundheit" the copy still exists: still out.
    let copy = node(&env, &home, "Gesundheit/Video.mp4").await.id;
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/nodes/{copy}"), None)
            .await
            .status,
        204
    );
    assert_eq!(round(&env).await.uploaded, 0);
    // Deleted for good: allowed again.
    assert_eq!(
        klaus
            .send("DELETE", &format!("/api/trash/{copy}"), None)
            .await
            .status,
        204
    );
    assert_eq!(round(&env).await.uploaded, 1);

    // "Haus" itself becomes "Nur lokal": everything of it leaves at once.
    assert_eq!(fake.keys().len(), 1);
    let r = klaus
        .send(
            "PUT",
            &format!("/api/nodes/{haus}/data-class"),
            Some(json!({"class": "local"})),
        )
        .await;
    assert_eq!(r.status, 200);
    // Before any round: marked, and never handed out again.
    assert_eq!(objects(&env).await, [("deleting".into(), "link".into())]);
    assert_eq!(get(&env, &path, OUTSIDE, None).await.status, StatusCode::OK);
    round(&env).await;
    assert!(fake.keys().is_empty() && objects(&env).await.is_empty());
    env.finish().await;
}

#[tokio::test]
async fn beliebt_neue_fassung_budget_alter() {
    let fake = FakeS3::start().await;
    let Some(env) = Env::with_s3(&fake).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    for (i, n) in ["A", "B", "C", "D"].iter().enumerate() {
        write(
            &dir.join(format!("Fotos/{n}.mov")),
            &data(10 + i as u8, 4_000_000),
        );
    }
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let id = |n: &str| {
        let (env, home, n) = (&env, &home, n.to_owned());
        async move { node(env, home, &format!("Fotos/{n}.mov")).await.id }
    };
    let (a, b, c, d) = (id("A").await, id("B").await, id("C").await, id("D").await);
    let dl = |n: i64| format!("/api/nodes/{n}/content");

    // Fetched from outside twice: mirrored; inside it does not count.
    get(&env, &dl(a), OUTSIDE, Some(&klaus)).await;
    get(&env, &dl(a), INSIDE, Some(&klaus)).await;
    assert_eq!(round(&env).await.uploaded, 0);
    get(&env, &dl(a), OUTSIDE, Some(&klaus)).await;
    get(&env, &dl(b), OUTSIDE, Some(&klaus)).await;
    get(&env, &dl(b), OUTSIDE, Some(&klaus)).await;
    // A continued download is the same download.
    let mut r = Request::builder()
        .uri(dl(c))
        .header("x-forwarded-for", OUTSIDE)
        .header("range", "bytes=100-");
    r = r.header(
        header::COOKIE,
        format!("xlrx_session={}", klaus.cookie.clone().unwrap()),
    );
    env.app
        .clone()
        .oneshot(r.body(Body::empty()).unwrap())
        .await
        .unwrap();
    get(&env, &dl(c), OUTSIDE, Some(&klaus)).await;
    assert_eq!(round(&env).await.uploaded, 2);
    assert_eq!(
        objects(&env).await,
        [
            ("ready".into(), "popular".into()),
            ("ready".into(), "popular".into())
        ]
    );
    assert_eq!(
        get(&env, &dl(a), OUTSIDE, Some(&klaus)).await.status,
        StatusCode::TEMPORARY_REDIRECT
    );
    // B is used later than A.
    assert_eq!(
        get(&env, &dl(b), OUTSIDE, Some(&klaus)).await.status,
        StatusCode::TEMPORARY_REDIRECT
    );

    // Budget 10 MB, 8 MB used: a link's file makes room, the least recently used goes.
    let (d_link, _) = link(&mut klaus, d, "view").await;
    assert_eq!(round(&env).await.uploaded, 1);
    assert_eq!(
        get(&env, &dl(a), OUTSIDE, Some(&klaus)).await.status,
        StatusCode::OK,
        "A ist gegangen"
    );
    assert_eq!(
        get(&env, &dl(b), OUTSIDE, Some(&klaus)).await.status,
        StatusCode::TEMPORARY_REDIRECT
    );
    assert_eq!(
        get(&env, &dl(d), OUTSIDE, Some(&klaus)).await.status,
        StatusCode::TEMPORARY_REDIRECT
    );
    assert_eq!(fake.keys().len(), 2);
    // Popular ones do not push anything out: A stays on the NAS for now.
    get(&env, &dl(a), OUTSIDE, Some(&klaus)).await;
    assert_eq!(round(&env).await.uploaded, 0);

    // A new version of D: the link's file is the new content, the old object goes.
    let cur = klaus.get(&format!("/api/nodes/{d}")).await.ok().clone();
    let r = klaus
        .send_bytes(
            "PUT",
            &format!("{}?base_rev={}", dl(d), cur["rev"]),
            data(30, 3_000_000),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(round(&env).await.uploaded, 1);
    assert_eq!(fake.keys().len(), 2);
    let g = get(&env, &dl(d), OUTSIDE, Some(&klaus)).await;
    let (_, body) = fetch(g.location()).await;
    assert!(body == data(30, 3_000_000), "die neue Fassung");

    // Unused for too long: popular objects go, a link's stays.
    sqlx::query(
        "UPDATE s3_objects SET uploaded_at = now() - interval '40 days', last_hit = now() - interval '40 days'",
    )
    .execute(&env.db.pool)
    .await
    .unwrap();
    sqlx::query("UPDATE remote_fetches SET at = now() - interval '40 days'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    round(&env).await;
    assert_eq!(objects(&env).await, [("ready".into(), "link".into())]);
    assert_eq!(fake.keys().len(), 1);
    // Ended: gone.
    klaus
        .send("DELETE", &format!("/api/links/{d_link}"), None)
        .await;
    round(&env).await;
    assert!(fake.keys().is_empty());
    env.finish().await;
}

#[tokio::test]
async fn aufraeumen_und_vorausladen() {
    let fake = FakeS3::start().await;
    let Some(env) = Env::with_s3(&fake).await else {
        return;
    };
    let (mut klaus, root_id, _, dir) = signed_in(&env, "klaus").await;
    write(&dir.join("Reise/Karte.pdf"), &data(40, 50_000));
    let home = root(&env, root_id).await;
    roots::scan(&env.state, &home).await.unwrap();
    let karte = node(&env, &home, "Reise/Karte.pdf").await.id;
    let s3 = env.state.s3.clone().unwrap();
    sqlx::query("UPDATE users SET is_admin = true WHERE username = 'klaus'")
        .execute(&env.db.pool)
        .await
        .unwrap();

    // Objects nobody knows of (an upload finished just before a crash): removed, but only those
    // under the cache's own prefix.
    s3.put("x/verwaist", bytes::Bytes::from_static(b"v"))
        .await
        .unwrap();
    s3.put("fremd/datei", bytes::Bytes::from_static(b"f"))
        .await
        .unwrap();
    assert_eq!(mirror::orphans(&env.state).await.unwrap(), 1);
    assert_eq!(fake.keys(), ["fremd/datei"]);
    // An upload interrupted by a crash: cleaned up.
    s3.put("x/halb", bytes::Bytes::from_static(b"h"))
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO s3_objects (content_hash, key, size, state, reason)
         VALUES ($1, 'x/halb', 50000, 'uploading', 'popular')",
    )
    .bind(hash_of(&env, karte).await)
    .execute(&env.db.pool)
    .await
    .unwrap();
    round(&env).await;
    assert!(objects(&env).await.is_empty());
    assert_eq!(fake.keys(), ["fremd/datei"]);

    // Prefetching: only for people who asked for it, their starred files.
    klaus
        .send("PUT", &format!("/api/nodes/{karte}/star"), None)
        .await;
    assert_eq!(round(&env).await.uploaded, 0);
    let r = klaus
        .send("PUT", "/api/me/prefetch", Some(json!({"on": true})))
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    assert_eq!(klaus.get("/api/me").await.ok()["prefetch"], true);
    assert_eq!(round(&env).await.uploaded, 1);
    assert_eq!(objects(&env).await, [("ready".into(), "prefetch".into())]);

    // The administration sees what is there, and can empty it (with a fresh second factor).
    let st = klaus.get("/api/admin/cache").await;
    assert_eq!(st.status, 200, "{}", st.body);
    let st = st.ok().clone();
    assert_eq!(
        (
            st["configured"].as_bool(),
            st["objects"].as_i64(),
            st["bytes"].as_i64()
        ),
        (Some(true), Some(1), Some(50_000))
    );
    assert_eq!(st["by_reason"]["prefetch"], 1);
    sqlx::query("UPDATE sessions SET step_up_at = now() - interval '1 day'")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = klaus.post("/api/admin/cache/clear", json!({})).await;
    assert_eq!(
        (r.status.as_u16(), r.err()),
        (403, "step_up_required".to_string())
    );
    sqlx::query("UPDATE sessions SET step_up_at = now()")
        .execute(&env.db.pool)
        .await
        .unwrap();
    let r = klaus.post("/api/admin/cache/clear", json!({})).await;
    assert_eq!(r.status, 204, "{}", r.body);
    sqlx::query("UPDATE users SET prefetch = false")
        .execute(&env.db.pool)
        .await
        .unwrap();
    round(&env).await;
    assert_eq!(fake.keys(), ["fremd/datei"]);
    env.finish().await;
}

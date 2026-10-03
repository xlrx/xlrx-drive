//! The S3 client against a real S3 implementation (s3s-fs checks every signature itself).

mod common;

use common::s3::FakeS3;
use http_body_util::{BodyExt, Empty};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use xlrx_server::s3::{S3, Throttle};

async fn fetch(url: &str, range: Option<&str>) -> (u16, Vec<u8>, String) {
    let client = Client::builder(TokioExecutor::new()).build_http::<Empty<bytes::Bytes>>();
    let mut req = hyper::Request::get(url);
    if let Some(r) = range {
        req = req.header("range", r);
    }
    let res = client
        .request(req.body(Empty::new()).unwrap())
        .await
        .unwrap();
    let status = res.status().as_u16();
    let disposition = res
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = res.into_body().collect().await.unwrap().to_bytes().to_vec();
    (status, body, disposition)
}

#[tokio::test]
async fn hoch_runter_weg() {
    let fake = FakeS3::start().await;
    let s3 = S3::new(fake.cfg.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();

    // Small: one request.
    let small = dir.path().join("klein");
    std::fs::write(&small, b"Hallo S3").unwrap();
    s3.put_file("x/klein", &small, &mut Throttle::new(None))
        .await
        .unwrap();
    assert_eq!(s3.size("x/klein").await.unwrap(), Some(8));
    assert_eq!(s3.size("x/fehlt").await.unwrap(), None);

    // Large: in parts (16 MiB + 16 MiB + 1 MiB).
    let big = dir.path().join("gross");
    let data: Vec<u8> = (0..33 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(&big, &data).unwrap();
    s3.put_file("x/gross", &big, &mut Throttle::new(None))
        .await
        .unwrap();
    assert_eq!(s3.size("x/gross").await.unwrap(), Some(data.len() as u64));

    let mut listed = s3.list("x/").await.unwrap();
    listed.sort();
    assert_eq!(
        listed,
        [
            ("x/gross".to_string(), data.len() as u64),
            ("x/klein".to_string(), 8)
        ]
    );

    // Presigned: fetched without credentials, ranges work; a changed signature does not.
    let url = s3.presign_get(
        "x/gross",
        std::time::Duration::from_secs(300),
        "attachment; filename=\"Film.mp4\"",
        "application/octet-stream",
    );
    let (status, body, _) = fetch(&url, None).await;
    assert_eq!(status, 200);
    assert!(body == data, "Inhalt unverändert");
    let (status, body, _) = fetch(&url, Some("bytes=100-109")).await;
    assert_eq!((status, body.as_slice()), (206, &data[100..110]));
    let forged = url.replace("Film.mp4", "Anders.mp4");
    assert_eq!(fetch(&forged, None).await.0, 403);

    s3.delete("x/klein").await.unwrap();
    s3.delete("x/klein").await.unwrap();
    assert_eq!(s3.size("x/klein").await.unwrap(), None);
    assert_eq!(fake.keys(), ["x/gross"]);

    // Wrong secret: refused.
    let mut wrong = fake.cfg.clone();
    wrong.secret_key = "falsch".into();
    let bad = S3::new(wrong).unwrap();
    assert!(
        bad.put("x/b", bytes::Bytes::from_static(b"b"))
            .await
            .is_err()
    );
}

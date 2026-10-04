//! The JSON forms of the API as clients see them (`crates/xlrx-wire`): answers are written from
//! the server's own types into `crates/xlrx-wire/tests/golden/`, requests written by xlrx-wire are
//! read with the server's types. Together with the round trip in xlrx-wire, server and client
//! cannot drift apart unnoticed. Rewrite with `XLRX_BLESS=1` after a deliberate change.

use std::path::PathBuf;

use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::Value;
use time::macros::datetime;
use xlrx_proto::{ContentHash, FileContent, Kind, Name, NodeId, Rev, Seq};
use xlrx_server::api::{devices, files, sync, uploads};
use xlrx_server::auth::device::Tokens;
use xlrx_server::error::ApiError;
use xlrx_server::files::access::Role;
use xlrx_sync::{LocalId, Origin, RemoteEntry, RemoteOp, RemoteResult};

fn golden(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../xlrx-wire/tests/golden")
        .join(name)
}

/// An answer: written by the server, compared as JSON value.
fn answer(name: &str, value: Value) {
    let path = golden(&format!("{name}.json"));
    let text = serde_json::to_string_pretty(&value).unwrap() + "\n";
    if std::env::var_os("XLRX_BLESS").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let stored: Value = serde_json::from_str(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} fehlt ({e}): XLRX_BLESS=1", path.display())),
    )
    .unwrap();
    assert_eq!(stored, value, "Form von {name} geändert");
}

/// A request written by xlrx-wire.
fn request(name: &str) -> String {
    let path = golden(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} fehlt ({e}): mit XLRX_BLESS=1 in xlrx-wire erzeugen",
            path.display()
        )
    })
}

fn hash(b: u8) -> ContentHash {
    ContentHash([b; 32])
}

fn entry() -> RemoteEntry {
    RemoteEntry {
        parent: NodeId(7),
        name: Name::new("Bericht.pdf").unwrap(),
        kind: Kind::File,
        content: Some(FileContent {
            hash: hash(0xab),
            size: 1234,
        }),
        rev: Rev(3),
    }
}

#[test]
fn antworten() {
    answer(
        "Changes",
        serde_json::to_value(sync::Changes {
            changes: vec![
                sync::Change {
                    node: NodeId(42),
                    state: Some(entry()),
                },
                sync::Change {
                    node: NodeId(43),
                    state: None,
                },
            ],
            cursor: 1001,
            more: false,
            cursor_tag: Some("0123456789abcdef".into()),
        })
        .unwrap(),
    );
    answer(
        "Changed",
        serde_json::to_value(vec![sync::Changed { root: 1, seq: 1001 }]).unwrap(),
    );
    answer(
        "Available",
        serde_json::to_value(sync::Available { available: true }).unwrap(),
    );
    answer(
        "OpLookup",
        serde_json::to_value(sync::OpLookup {
            result: RemoteResult::Created {
                node: NodeId(44),
                rev: Rev(1),
                seq: Seq(1002),
            },
            op: Some(RemoteOp::CreateDir {
                parent: NodeId(7),
                name: Name::new("Neu").unwrap(),
                source: LocalId(99),
                origin: Origin::New,
            }),
        })
        .unwrap(),
    );
    answer(
        "UploadInfo",
        serde_json::to_value(uploads::UploadInfo {
            id: uuid::Uuid::from_u128(0x0123_4567_89ab_cdef_0123_4567_89ab_cdef),
            size: 20_000_000,
            received: vec![[0, 8_388_608], [16_777_216, 20_000_000]],
            state: "open".into(),
            expires_at: datetime!(2026-10-05 12:00:00 UTC),
        })
        .unwrap(),
    );
    answer(
        "Tokens",
        serde_json::to_value(Tokens {
            access_token: "zugriff".into(),
            refresh_token: "erneuern".into(),
            expires_in: 900,
            device_id: 3,
            confirm_until: datetime!(2027-01-01 00:00:00 UTC),
        })
        .unwrap(),
    );
    for (name, scanned_at) in [
        ("RootInfo", Some(datetime!(2026-10-04 08:30:00 UTC))),
        ("RootInfo-nie-abgeglichen", None),
    ] {
        answer(
            name,
            serde_json::to_value(files::RootInfo {
                id: 1,
                kind: "home".into(),
                name: "Meine Ablage".into(),
                node_id: 7,
                role: Role::Owner,
                scanned_at,
            })
            .unwrap(),
        );
    }
}

#[tokio::test]
async fn fehler() {
    for (name, err) in [
        (
            "ErrorBody-mit-Grund",
            ApiError::Refused("op_mismatch", "Andere Operation.".into()),
        ),
        ("ErrorBody", ApiError::bad("Ungültiger Hash.")),
    ] {
        let body = err.into_response().into_body().collect().await.unwrap();
        answer(name, serde_json::from_slice(&body.to_bytes()).unwrap());
    }
}

#[test]
fn anfragen() {
    let q: sync::ChangesQuery = serde_urlencoded::from_str(&request("ChangesQuery.query")).unwrap();
    assert_eq!(
        (q.root, q.cursor, q.limit, q.check.as_deref()),
        (1, 1001, Some(10_000), Some("0123456789abcdef"))
    );
    let q: sync::ChangesQuery =
        serde_urlencoded::from_str(&request("ChangesQuery-erster-Abruf.query")).unwrap();
    assert_eq!((q.root, q.cursor, q.limit, q.check), (1, 0, None, None));
    let q: sync::ContentQuery = serde_urlencoded::from_str(&request("ContentQuery.query")).unwrap();
    assert_eq!(q.size, Some(1234));
    let q: sync::OpResultQuery =
        serde_urlencoded::from_str(&request("OpResultQuery.query")).unwrap();
    assert_eq!(q.with_op.as_deref(), Some("true"));
    let r: sync::OpRequest = serde_json::from_str(&request("OpRequest.json")).unwrap();
    assert_eq!(
        (r.device.as_str(), r.op_id, r.cursor, r.check.as_deref()),
        (
            "MacBook von Klaus",
            4_294_967_297,
            1001,
            Some("0123456789abcdef")
        )
    );
    assert!(matches!(r.op, RemoteOp::CreateFile { .. }));
    let q: uploads::PartQuery = serde_urlencoded::from_str(&request("PartQuery.query")).unwrap();
    assert_eq!(q.offset, 8_388_608);
    for (file, size) in [
        ("CreateUpload-neu.json", 20_000_000),
        ("CreateUpload-ersetzen.json", 20_000_000),
        ("CreateUpload-inhalt.json", 9_000_000),
    ] {
        let r: uploads::CreateReq = serde_json::from_str(&request(file)).unwrap();
        assert_eq!(r.size, size, "{file}");
    }
    let r: uploads::CommitReq = serde_json::from_str(&request("CommitUpload.json")).unwrap();
    assert_eq!(r.hash.as_deref().map(str::len), Some(64));
    for file in ["TokenRequest-code.json", "TokenRequest-erneuern.json"] {
        let _: devices::TokenReq = serde_json::from_str(&request(file)).unwrap();
    }
}

//! Golden forms: answers come from the server's types (`crates/xlrx-server/tests/wire_formen.rs`)
//! and must read back into these types unchanged; requests are written here and the server's test
//! reads them with its types. Rewrite requests with `XLRX_BLESS=1` after a deliberate change.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use xlrx_proto::{ContentHash, FileContent, Name, NodeId};
use xlrx_sync::{Fingerprint, LocalId, Origin, RemoteOp};
use xlrx_wire::devices::{TokenRequest, Tokens};
use xlrx_wire::error::ErrorBody;
use xlrx_wire::roots::{Role, RootInfo};
use xlrx_wire::sync::{
    Available, Changed, Changes, ChangesQuery, ContentQuery, OpLookup, OpRequest, OpResultQuery,
};
use xlrx_wire::uploads::{CommitUpload, CreateUpload, PartQuery, UploadInfo, UploadTarget};

fn path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

fn read(name: &str) -> String {
    std::fs::read_to_string(path(name)).unwrap_or_else(|e| panic!("{name} fehlt ({e})"))
}

fn bless() -> bool {
    std::env::var_os("XLRX_BLESS").is_some()
}

/// An answer written by the server reads into the client's type and back to the same JSON.
fn answer<T: Serialize + DeserializeOwned>(name: &str) -> T {
    let stored: Value = serde_json::from_str(&read(&format!("{name}.json"))).unwrap();
    let value: T = serde_json::from_value(stored.clone())
        .unwrap_or_else(|e| panic!("{name} passt nicht zum Client-Typ: {e}"));
    assert_eq!(serde_json::to_value(&value).unwrap(), stored, "{name}");
    value
}

fn request_json<T: Serialize>(name: &str, value: &T) {
    let text = serde_json::to_string_pretty(value).unwrap() + "\n";
    if bless() {
        std::fs::write(path(name), text).unwrap();
    } else {
        let stored: Value = serde_json::from_str(&read(name)).unwrap();
        assert_eq!(stored, serde_json::to_value(value).unwrap(), "{name}");
    }
}

fn request_query<T: Serialize>(name: &str, value: &T) {
    let text = serde_urlencoded::to_string(value).unwrap();
    if bless() {
        std::fs::write(path(name), &text).unwrap();
    } else {
        assert_eq!(read(name), text, "{name}");
    }
}

#[test]
fn antworten_des_servers() {
    let changes: Changes = answer("Changes");
    assert_eq!(changes.changes.len(), 2);
    assert!(changes.changes[1].state.is_none());
    assert_eq!(changes.cursor_tag.as_deref(), Some("0123456789abcdef"));
    let changed: Vec<Changed> = answer("Changed");
    assert_eq!(changed, vec![Changed { root: 1, seq: 1001 }]);
    let a: Available = answer("Available");
    assert!(a.available);
    let lookup: OpLookup = answer("OpLookup");
    assert!(matches!(lookup.op, Some(RemoteOp::CreateDir { .. })));
    let up: UploadInfo = answer("UploadInfo");
    assert_eq!(up.received, vec![[0, 8_388_608], [16_777_216, 20_000_000]]);
    let t: Tokens = answer("Tokens");
    assert_eq!(t.device_id, 3);
    let r: RootInfo = answer("RootInfo");
    assert_eq!((r.role, r.node_id), (Role::Owner, 7));
    assert!(r.scanned_at.is_some());
    let r: RootInfo = answer("RootInfo-nie-abgeglichen");
    assert!(r.scanned_at.is_none());
    let e: ErrorBody = answer("ErrorBody-mit-Grund");
    assert_eq!(
        e.reason.as_deref(),
        Some(xlrx_wire::error::reason::OP_MISMATCH)
    );
    let e: ErrorBody = answer("ErrorBody");
    assert_eq!(e.reason, None);
}

#[test]
fn anfragen_des_clients() {
    let tag = Some("0123456789abcdef".to_owned());
    request_query(
        "ChangesQuery.query",
        &ChangesQuery {
            root: 1,
            cursor: 1001,
            limit: Some(10_000),
            check: tag.clone(),
        },
    );
    request_query(
        "ChangesQuery-erster-Abruf.query",
        &ChangesQuery {
            root: 1,
            cursor: 0,
            limit: None,
            check: None,
        },
    );
    request_query("ContentQuery.query", &ContentQuery { size: Some(1234) });
    request_query(
        "OpResultQuery.query",
        &OpResultQuery {
            with_op: Some("true".into()),
        },
    );
    request_query("PartQuery.query", &PartQuery { offset: 8_388_608 });
    request_json(
        "OpRequest.json",
        &OpRequest {
            device: "MacBook von Klaus".into(),
            op_id: (1 << 32) + 1,
            op: RemoteOp::CreateFile {
                parent: NodeId(7),
                name: Name::new("Bericht.pdf").unwrap(),
                content: FileContent {
                    hash: ContentHash([0xab; 32]),
                    size: 1234,
                },
                source: LocalId(99),
                fp: Fingerprint {
                    size: 1234,
                    mtime_ns: 1_759_500_000_000_000_000,
                    ctime_ns: 1_759_500_000_000_000_000,
                },
                origin: Origin::New,
            },
            cursor: 1001,
            check: tag,
        },
    );
    for (name, size, target) in [
        (
            "CreateUpload-neu.json",
            20_000_000,
            UploadTarget::New {
                parent_id: 7,
                name: "Film.mov".into(),
                keep_both: false,
            },
        ),
        (
            "CreateUpload-ersetzen.json",
            20_000_000,
            UploadTarget::Replace {
                node_id: 42,
                base_rev: 3,
            },
        ),
        ("CreateUpload-inhalt.json", 9_000_000, UploadTarget::Content),
    ] {
        request_json(
            name,
            &CreateUpload {
                size,
                target,
                mtime_ms: None,
            },
        );
    }
    request_json(
        "CommitUpload.json",
        &CommitUpload {
            hash: Some("ab".repeat(32)),
            keep_both: None,
        },
    );
    request_json(
        "TokenRequest-code.json",
        &TokenRequest::AuthorizationCode {
            code: "code".into(),
            code_verifier: "v".repeat(64),
            redirect_uri: "xlrx://auth".into(),
            refresh_token: None,
        },
    );
    request_json(
        "TokenRequest-erneuern.json",
        &TokenRequest::RefreshToken {
            refresh_token: "erneuern".into(),
        },
    );
}

#[test]
fn fehlergruende_wie_beim_server() {
    // Reasons are part of the contract; the server's spelling lives in its error constructors.
    use xlrx_wire::error::reason::*;
    for r in [
        TOKEN_INVALID,
        REVOKED,
        REAUTH_REQUIRED,
        CODE_INVALID,
        CONTENT_MISSING,
        OP_MISMATCH,
        CURSOR_INVALID,
        DISK_FULL,
    ] {
        assert!(r.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{r}");
    }
}

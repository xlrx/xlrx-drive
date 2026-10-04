//! `local.sqlite` of a sync folder (ADR 0002 §6): what the client knows about its own disk —
//! the hash cache, intents written before irreversible steps, and its trash. Separate from the
//! engine's state so that executing never waits for an engine commit; kept on a rebuild.
//!
//! Durability: WAL with `synchronous = FULL` (and `fullfsync` on Apple): when a write returns, it
//! survives a crash. Unsigned 64-bit values (local IDs) are stored bit for bit as SQLite integers.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use xlrx_chunk::{CacheEntry, Fingerprint as ChunkFp};
use xlrx_proto::{ContentHash, FileContent};
use xlrx_sync::{Fingerprint, LocalId};

/// Schema version (`PRAGMA user_version`).
const SCHEMA: i64 = 1;

const CREATE: &str = "
CREATE TABLE hash_cache (
    local        INTEGER PRIMARY KEY,
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    ctime_ns     INTEGER NOT NULL,
    hashed_at_ns INTEGER NOT NULL,
    hash         BLOB NOT NULL
) STRICT;
CREATE TABLE intent (
    id          INTEGER PRIMARY KEY,
    op_id       INTEGER NOT NULL,
    kind        TEXT NOT NULL CHECK (kind IN ('download', 'replace', 'delete')),
    dir_path    BLOB NOT NULL,
    dir_ino     INTEGER NOT NULL,
    target_name BLOB NOT NULL,
    temp_name   BLOB,
    temp_ino    INTEGER,
    orig_ino    INTEGER,
    trash_name  BLOB,
    expect      TEXT,
    phase       TEXT NOT NULL,
    created_ms  INTEGER NOT NULL
) STRICT;
CREATE TABLE trash (
    id         INTEGER PRIMARY KEY,
    trash_name BLOB NOT NULL UNIQUE,
    orig_path  BLOB NOT NULL,
    ino        INTEGER,
    node       INTEGER,
    hash       BLOB,
    size       INTEGER,
    reason     TEXT NOT NULL CHECK (reason IN ('delete', 'replaced', 'junk', 'recovered')),
    trashed_ms INTEGER NOT NULL
) STRICT;
CREATE TABLE transfer (
    op_id      INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL,
    hash       BLOB NOT NULL,
    size       INTEGER NOT NULL,
    upload_id  TEXT,
    created_ms INTEGER NOT NULL
) STRICT;
";

/// Unsigned to SQLite integer and back, bit for bit.
fn sql(v: u64) -> i64 {
    v as i64
}

fn unsql(v: i64) -> u64 {
    v as u64
}

/// Why something lies in the client's trash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrashReason {
    /// A local deletion the engine planned (`LocalOp::DeleteFile`).
    Delete,
    /// The original a `Replace` swapped out.
    Replaced,
    /// Metadata junk left in a folder being deleted (`.DS_Store` …).
    Junk,
    /// Found after a crash without a matching intent: kept, never removed.
    Recovered,
}

impl TrashReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::Delete => "delete",
            Self::Replaced => "replaced",
            Self::Junk => "junk",
            Self::Recovered => "recovered",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "delete" => Self::Delete,
            "replaced" => Self::Replaced,
            "junk" => Self::Junk,
            "recovered" => Self::Recovered,
            _ => return None,
        })
    }
}

/// A file in the client's trash: under `trash_name` in the trash directory, from `orig_path`
/// (relative to the sync folder, raw bytes of the names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrashItem {
    pub id: i64,
    pub trash_name: Vec<u8>,
    pub orig_path: Vec<u8>,
    pub ino: Option<u64>,
    pub node: Option<u64>,
    pub content: Option<FileContent>,
    pub reason: TrashReason,
    pub trashed_ms: i64,
}

pub struct LocalStore {
    conn: Connection,
}

impl LocalStore {
    /// Opens (or creates) the store. A store written by a newer version is refused.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        #[cfg(target_vendor = "apple")]
        {
            conn.pragma_update(None, "fullfsync", "ON")?;
            conn.pragma_update(None, "checkpoint_fullfsync", "ON")?;
        }
        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match version {
            0 => {
                let tx = conn.unchecked_transaction()?;
                tx.execute_batch(CREATE)?;
                tx.pragma_update(None, "user_version", SCHEMA)?;
                tx.commit()?;
            }
            SCHEMA => {}
            newer => {
                return Err(rusqlite::Error::InvalidParameterName(format!(
                    "local.sqlite hat Schema {newer}, diese Version kennt {SCHEMA}"
                )));
            }
        }
        Ok(Self { conn })
    }

    /// The cached content of an object, if its fingerprint is exactly the one hashed and was
    /// already older than the unsafe window when it was hashed (`xlrx_chunk::CacheEntry::is_racy`).
    pub fn cached(&self, id: LocalId, fp: Fingerprint) -> rusqlite::Result<Option<FileContent>> {
        let row: Option<(i64, i64, i64, i64, Vec<u8>)> = self
            .conn
            .query_row(
                "SELECT size, mtime_ns, ctime_ns, hashed_at_ns, hash FROM hash_cache WHERE local = ?1",
                [sql(id.0)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((size, mtime_ns, ctime_ns, hashed_at_ns, hash)) = row else {
            return Ok(None);
        };
        let Ok(hash) = <[u8; 32]>::try_from(hash.as_slice()) else {
            return Ok(None);
        };
        let entry = CacheEntry {
            fingerprint: ChunkFp {
                size: unsql(size),
                mtime_ns,
                ctime_ns,
            },
            hashed_at_ns,
            content: FileContent {
                hash: ContentHash(hash),
                size: unsql(size),
            },
        };
        let same = entry.fingerprint.size == fp.size
            && entry.fingerprint.mtime_ns == fp.mtime_ns
            && entry.fingerprint.ctime_ns == fp.ctime_ns;
        Ok((same && !entry.is_racy()).then_some(entry.content))
    }

    /// Remembers a hash. `hashed_at_ns` is the wall-clock time taken *before* reading the file.
    pub fn remember(
        &self,
        id: LocalId,
        fp: Fingerprint,
        hashed_at_ns: i64,
        content: FileContent,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO hash_cache (local, size, mtime_ns, ctime_ns, hashed_at_ns, hash)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (local) DO UPDATE SET size = ?2, mtime_ns = ?3, ctime_ns = ?4,
                 hashed_at_ns = ?5, hash = ?6",
            params![
                sql(id.0),
                sql(fp.size),
                fp.mtime_ns,
                fp.ctime_ns,
                hashed_at_ns,
                content.hash.0.as_slice()
            ],
        )?;
        Ok(())
    }

    pub fn forget(&self, id: LocalId) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM hash_cache WHERE local = ?1", [sql(id.0)])?;
        Ok(())
    }

    /// Records a file moved into the trash. Written after the move, before anything else
    /// happens to it.
    pub fn add_trash(&self, item: &TrashItem) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO trash (trash_name, orig_path, ino, node, hash, size, reason, trashed_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                item.trash_name,
                item.orig_path,
                item.ino.map(sql),
                item.node.map(sql),
                item.content.map(|c| c.hash.0.to_vec()),
                item.content.map(|c| sql(c.size)),
                item.reason.as_str(),
                item.trashed_ms
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Everything in the trash, oldest first.
    pub fn trash(&self) -> rusqlite::Result<Vec<TrashItem>> {
        let mut st = self.conn.prepare(
            "SELECT id, trash_name, orig_path, ino, node, hash, size, reason, trashed_ms
               FROM trash ORDER BY trashed_ms, id",
        )?;
        let rows = st.query_map([], |r| {
            let hash: Option<Vec<u8>> = r.get(5)?;
            let size: Option<i64> = r.get(6)?;
            let reason: String = r.get(7)?;
            Ok(TrashItem {
                id: r.get(0)?,
                trash_name: r.get(1)?,
                orig_path: r.get(2)?,
                ino: r.get::<_, Option<i64>>(3)?.map(unsql),
                node: r.get::<_, Option<i64>>(4)?.map(unsql),
                content: match (hash, size) {
                    (Some(h), Some(s)) => {
                        <[u8; 32]>::try_from(h.as_slice())
                            .ok()
                            .map(|h| FileContent {
                                hash: ContentHash(h),
                                size: unsql(s),
                            })
                    }
                    _ => None,
                },
                reason: TrashReason::parse(&reason).unwrap_or(TrashReason::Recovered),
                trashed_ms: r.get(8)?,
            })
        })?;
        rows.collect()
    }

    /// Forgets a trash entry (after its file was removed for good or restored).
    pub fn remove_trash(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM trash WHERE id = ?1", [id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    fn fp(size: u64, t: i64) -> Fingerprint {
        Fingerprint {
            size,
            mtime_ns: t,
            ctime_ns: t,
        }
    }

    fn content(b: u8, size: u64) -> FileContent {
        FileContent {
            hash: ContentHash([b; 32]),
            size,
        }
    }

    #[test]
    fn hash_cache_nur_bei_genauem_fingerprint_und_altem_stand() {
        let t = tempfile::tempdir().unwrap();
        let s = LocalStore::open(&t.path().join("local.sqlite")).unwrap();
        let id = LocalId(u64::MAX - 5); // above i64::MAX: stored bit for bit
        s.remember(id, fp(10, 100 * S), 200 * S, content(1, 10))
            .unwrap();
        assert_eq!(s.cached(id, fp(10, 100 * S)).unwrap(), Some(content(1, 10)));
        // Any difference in the fingerprint: no hit.
        assert_eq!(s.cached(id, fp(11, 100 * S)).unwrap(), None);
        assert_eq!(s.cached(id, fp(10, 100 * S + 1)).unwrap(), None);
        assert_eq!(s.cached(LocalId(1), fp(10, 100 * S)).unwrap(), None);
        // Hashed less than 2 s after the last change: never trusted.
        s.remember(id, fp(10, 100 * S), 101 * S, content(2, 10))
            .unwrap();
        assert_eq!(s.cached(id, fp(10, 100 * S)).unwrap(), None);
        s.forget(id).unwrap();
        s.remember(id, fp(10, 100 * S), 300 * S, content(3, 10))
            .unwrap();
        drop(s);
        // Survives reopening.
        let s = LocalStore::open(&t.path().join("local.sqlite")).unwrap();
        assert_eq!(s.cached(id, fp(10, 100 * S)).unwrap(), Some(content(3, 10)));
    }

    #[test]
    fn papierkorb_zeilen() {
        let t = tempfile::tempdir().unwrap();
        let s = LocalStore::open(&t.path().join("local.sqlite")).unwrap();
        let item = TrashItem {
            id: 0,
            trash_name: b"2026-10-04/17-a1b2".to_vec(),
            orig_path: "Projekte/Bericht \u{00C4}.pdf".as_bytes().to_vec(),
            ino: Some(u64::MAX - 1),
            node: Some(42),
            content: Some(content(7, 99)),
            reason: TrashReason::Replaced,
            trashed_ms: 1_759_500_000_000,
        };
        let id = s.add_trash(&item).unwrap();
        let all = s.trash().unwrap();
        assert_eq!(all, vec![TrashItem { id, ..item.clone() }]);
        // A name lies in the trash only once.
        assert!(s.add_trash(&item).is_err());
        s.remove_trash(id).unwrap();
        assert!(s.trash().unwrap().is_empty());
    }

    #[test]
    fn neueres_schema_wird_abgelehnt() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("local.sqlite");
        drop(LocalStore::open(&p).unwrap());
        Connection::open(&p)
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(LocalStore::open(&p).is_err());
    }
}

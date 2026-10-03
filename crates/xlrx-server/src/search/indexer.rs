//! Keeps the index in step with the database: follows the journal (every change to a node) and
//! the extracted texts, both after a cursor stored in the index's own commits.
//!
//! Every round reads the *current* state of the nodes it touches, so it never matters how many
//! changes a node went through in between: the last journal entry always comes after the last
//! change and leads to the current state. Moving a directory to another parent changes the
//! ancestors of everything below it; such entries refresh the whole subtree.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};
use time::OffsetDateTime;
use tokio::sync::watch;

use super::Shared;
use super::schema::{self, Category, Fields};

/// Journal entries (and texts) per round.
const BATCH: i64 = 2000;
/// Nodes whose documents are built at once (texts can be large).
const CHUNK: usize = 200;
/// Without a notification, the database is looked at this often (texts from a worker in another
/// process, lost notifications).
const POLL: Duration = Duration::from_secs(5);
/// Memory for documents not yet written to a segment.
const WRITER_HEAP: usize = 64 * 1024 * 1024;

/// How far the index got, stored with every commit.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct Cursor {
    pub schema: u32,
    /// Last journal sequence applied.
    pub journal: i64,
    /// Last text sequence applied.
    pub text: i64,
}

/// Opens the index in `dir`, or creates it afresh (nothing there, damaged, other schema).
pub fn open(dir: &Path) -> Result<(Index, Cursor), String> {
    let (schema_def, _) = schema::build();
    if let Ok(index) = Index::open_in_dir(dir) {
        let cursor = index
            .load_metas()
            .ok()
            .and_then(|m| m.payload)
            .and_then(|p| serde_json::from_str::<Cursor>(&p).ok());
        match cursor {
            Some(c) if c.schema == schema::VERSION && index.schema() == schema_def => {
                schema::register(&index);
                return Ok((index, c));
            }
            _ => tracing::info!(dir = %dir.display(), "Suchindex wird neu aufgebaut"),
        }
    }
    // Only derived data lives here; everything comes back from the database.
    if dir.exists() {
        std::fs::remove_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let index = Index::create_in_dir(dir, schema_def).map_err(|e| format!("Suchindex: {e}"))?;
    schema::register(&index);
    Ok((
        index,
        Cursor {
            schema: schema::VERSION,
            ..Cursor::default()
        },
    ))
}

pub fn writer(index: &Index) -> Result<IndexWriter, String> {
    index
        .writer_with_num_threads(1, WRITER_HEAP)
        .map_err(|e| format!("Suchindex: {e}"))
}

pub async fn run(
    db: PgPool,
    sh: Arc<Shared>,
    writer: IndexWriter,
    mut cursor: Cursor,
    mut live: watch::Receiver<i64>,
    mut stop: watch::Receiver<bool>,
) {
    let writer = Arc::new(Mutex::new(writer));
    loop {
        if *stop.borrow() {
            return;
        }
        let more = match round(&db, &sh, &writer, &mut cursor).await {
            Ok(more) => more,
            Err(e) => {
                tracing::warn!(error = %e, "Suchindex: Aktualisieren fehlgeschlagen");
                false
            }
        };
        if more {
            continue;
        }
        tokio::select! {
            _ = changed(&mut live) => {}
            () = sh.wake.notified() => {}
            () = tokio::time::sleep(POLL) => {}
            _ = stop.changed() => return,
        }
    }
}

async fn changed(rx: &mut watch::Receiver<i64>) {
    if rx.changed().await.is_err() {
        std::future::pending::<()>().await;
    }
}

#[derive(sqlx::FromRow)]
struct Entry {
    seq: i64,
    node_id: i64,
    op: String,
    reparented: Option<bool>,
    kind: String,
}

#[derive(sqlx::FromRow, Clone, Debug)]
struct Row {
    id: i64,
    root_id: i64,
    parent_id: Option<i64>,
    name: String,
    kind: String,
    content_hash: Option<Vec<u8>>,
    mtime: OffsetDateTime,
    deleted_at: Option<OffsetDateTime>,
    /// Directories above, from the root directory down.
    anc: Vec<i64>,
}

#[derive(sqlx::FromRow)]
struct Text {
    hash: Vec<u8>,
    lang: Option<String>,
    text: String,
}

/// Applies the next batch of changes. True if more are waiting.
async fn round(
    db: &PgPool,
    sh: &Arc<Shared>,
    writer: &Arc<Mutex<IndexWriter>>,
    cursor: &mut Cursor,
) -> Result<bool, String> {
    let entries: Vec<Entry> = sqlx::query_as(
        "SELECT j.seq, j.node_id, j.op, j.reparented, n.kind
           FROM journal j JOIN nodes n ON n.id = j.node_id
          WHERE j.seq > $1 ORDER BY j.seq LIMIT $2",
    )
    .bind(cursor.journal)
    .bind(BATCH)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())?;
    let texts: Vec<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, hash FROM content_text WHERE seq > $1 ORDER BY seq LIMIT $2")
            .bind(cursor.text)
            .bind(BATCH)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?;
    if entries.is_empty() && texts.is_empty() {
        return Ok(false);
    }
    let mut next = *cursor;
    if let Some(e) = entries.last() {
        next.journal = e.seq;
    }
    if let Some((seq, _)) = texts.last() {
        next.text = *seq;
    }

    let mut ids: HashSet<i64> = entries.iter().map(|e| e.node_id).collect();
    let subtrees: HashSet<i64> = entries
        .iter()
        .filter(|e| e.kind == "dir" && e.op == "move" && e.reparented != Some(false))
        .map(|e| e.node_id)
        .collect();
    if !texts.is_empty() {
        let hashes: Vec<Vec<u8>> = texts.into_iter().map(|(_, h)| h).collect();
        let with_text: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM nodes WHERE content_hash = ANY($1) AND deleted_at IS NULL",
        )
        .bind(&hashes)
        .fetch_all(db)
        .await
        .map_err(|e| e.to_string())?;
        ids.extend(with_text);
    }

    let ids: Vec<i64> = ids.into_iter().collect();
    let mut rows: HashMap<i64, Row> = load(db, &ids)
        .await?
        .into_iter()
        .map(|r| (r.id, r))
        .collect();
    for dir in &subtrees {
        let Some(d) = rows.get(dir).filter(|d| d.deleted_at.is_none()) else {
            continue;
        };
        let mut anc = d.anc.clone();
        anc.push(d.id);
        for r in subtree(db, d.id, &anc).await? {
            rows.insert(r.id, r);
        }
    }
    // Nodes gone from the database altogether only need their document removed.
    let mut gone: Vec<i64> = ids
        .iter()
        .copied()
        .filter(|i| !rows.contains_key(i))
        .collect();
    let all: Vec<Row> = rows.into_values().collect();
    let total = all.len();
    for chunk in all.chunks(CHUNK) {
        let docs = documents(db, &sh.fields, chunk).await?;
        let deletes: Vec<i64> = chunk.iter().map(|r| r.id).chain(gone.drain(..)).collect();
        let (w, f) = (writer.clone(), sh.fields);
        blocking(move || {
            let w = w.lock().unwrap_or_else(|p| p.into_inner());
            for id in deletes {
                w.delete_term(Term::from_field_u64(f.id, id as u64));
            }
            for d in docs {
                w.add_document(d).map_err(|e| e.to_string())?;
            }
            Ok(())
        })
        .await?;
    }
    if !gone.is_empty() {
        let (w, f) = (writer.clone(), sh.fields);
        blocking(move || {
            let w = w.lock().unwrap_or_else(|p| p.into_inner());
            for id in gone {
                w.delete_term(Term::from_field_u64(f.id, id as u64));
            }
            Ok(())
        })
        .await?;
    }

    let (w, reader) = (writer.clone(), sh.reader.clone());
    let payload = serde_json::to_string(&next).map_err(|e| e.to_string())?;
    blocking(move || {
        let mut w = w.lock().unwrap_or_else(|p| p.into_inner());
        let mut commit = w.prepare_commit().map_err(|e| e.to_string())?;
        commit.set_payload(&payload);
        commit.commit().map_err(|e| e.to_string())?;
        reader.reload().map_err(|e| e.to_string())
    })
    .await?;
    *cursor = next;
    sh.indexed.send_replace(next);
    tracing::debug!(
        journal = next.journal,
        text = next.text,
        nodes = total,
        "Suchindex aktualisiert"
    );
    Ok(true)
}

async fn blocking<F>(f: F) -> Result<(), String>
where
    F: FnOnce() -> Result<(), String> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("Suchindex: {e}"))?
}

/// Current state of the given nodes, with their ancestors.
async fn load(db: &PgPool, ids: &[i64]) -> Result<Vec<Row>, String> {
    sqlx::query_as(
        "WITH RECURSIVE up AS (
            SELECT n.id AS node, n.parent_id AS anc, 0 AS depth FROM nodes n WHERE n.id = ANY($1)
            UNION ALL
            SELECT up.node, p.parent_id, up.depth + 1 FROM up JOIN nodes p ON p.id = up.anc
             WHERE p.parent_id IS NOT NULL AND up.depth < 1000
         ), a AS (
            SELECT node, array_agg(anc ORDER BY depth DESC) AS anc FROM up
             WHERE anc IS NOT NULL GROUP BY node
         )
         SELECT n.id, n.root_id, n.parent_id, n.name, n.kind, n.content_hash,
                coalesce(n.mtime, n.created_at) AS mtime, n.deleted_at,
                coalesce(a.anc, '{}') AS anc
           FROM nodes n LEFT JOIN a ON a.node = n.id
          WHERE n.id = ANY($1)",
    )
    .bind(ids)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())
}

/// Everything live below a directory whose own ancestors (itself included) are `anc`.
async fn subtree(db: &PgPool, dir: i64, anc: &[i64]) -> Result<Vec<Row>, String> {
    sqlx::query_as(
        "WITH RECURSIVE t AS (
            SELECT n.id, $2::bigint[] AS anc, 0 AS depth FROM nodes n
             WHERE n.parent_id = $1 AND n.deleted_at IS NULL
            UNION ALL
            SELECT n.id, t.anc || t.id, t.depth + 1 FROM nodes n JOIN t ON n.parent_id = t.id
             WHERE n.deleted_at IS NULL AND t.depth < 1000
         )
         SELECT n.id, n.root_id, n.parent_id, n.name, n.kind, n.content_hash,
                coalesce(n.mtime, n.created_at) AS mtime, n.deleted_at, t.anc
           FROM t JOIN nodes n ON n.id = t.id",
    )
    .bind(dir)
    .bind(anc)
    .fetch_all(db)
    .await
    .map_err(|e| e.to_string())
}

/// Documents for the live nodes among `rows` (root directories are not searchable themselves).
async fn documents(db: &PgPool, f: &Fields, rows: &[Row]) -> Result<Vec<TantivyDocument>, String> {
    let live: Vec<&Row> = rows
        .iter()
        .filter(|r| r.deleted_at.is_none() && r.parent_id.is_some())
        .collect();
    let hashes: Vec<Vec<u8>> = live
        .iter()
        .filter_map(|r| r.content_hash.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let texts: HashMap<Vec<u8>, Text> = if hashes.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, Text>("SELECT hash, lang, text FROM content_text WHERE hash = ANY($1)")
            .bind(&hashes)
            .fetch_all(db)
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|t| (t.hash.clone(), t))
            .collect()
    };
    Ok(live
        .into_iter()
        .map(|r| {
            let text = r.content_hash.as_ref().and_then(|h| texts.get(h));
            document(f, r, text)
        })
        .collect())
}

fn document(f: &Fields, r: &Row, text: Option<&Text>) -> TantivyDocument {
    let is_dir = r.kind == "dir";
    let ext = if is_dir {
        String::new()
    } else {
        schema::extension(&r.name)
    };
    let mut d = TantivyDocument::default();
    d.add_u64(f.id, r.id as u64);
    d.add_u64(f.root, r.root_id as u64);
    for a in &r.anc {
        d.add_u64(f.anc, *a as u64);
    }
    d.add_u64(f.cat, Category::of(is_dir, &ext).code());
    if !ext.is_empty() {
        d.add_text(f.ext, &ext);
    }
    d.add_i64(f.mtime, r.mtime.unix_timestamp());
    for field in [f.name, f.name_de, f.name_en, f.name_pre] {
        d.add_text(field, &r.name);
    }
    if let Some(t) = text {
        let fields: &[_] = match t.lang.as_deref() {
            Some("deu") => &[f.text_de],
            Some("eng") => &[f.text_en],
            Some(_) => &[f.text],
            // Unsure (short texts, mixed): both stemmers.
            None => &[f.text_de, f.text_en],
        };
        for field in fields {
            d.add_text(*field, &t.text);
        }
    }
    d
}

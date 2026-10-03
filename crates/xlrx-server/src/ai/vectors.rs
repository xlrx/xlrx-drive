//! Vectors in pgvector (PLAN 6.3): `halfvec` in one table, one partial HNSW index per space and
//! model.
//!
//! - **cloud** (many vectors): the index holds binary-quantized vectors (1 bit per dimension:
//!   128 bytes for 1024 dimensions), the best candidates are then ranked again with the full
//!   `halfvec`. So millions of vectors fit the NAS's memory.
//! - **local**, **clip** (few vectors, smaller models whose values are not centered around zero
//!   and quantize badly): the index holds the `halfvec` itself.

use sqlx::PgPool;
use sqlx::postgres::PgConnection;

use super::Space;

/// Candidates looked at per wanted hit when the index is quantized.
const OVERSAMPLE: usize = 4;

/// Scales a vector to length 1 (cosine distance then compares directions only). False for a
/// vector without direction (all zero) or with invalid numbers.
pub fn normalize(v: &mut [f32]) -> bool {
    let norm = v
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    if !norm.is_finite() || norm < 1e-12 {
        return false;
    }
    for x in v.iter_mut() {
        *x = (f64::from(*x) / norm) as f32;
    }
    true
}

/// The text form pgvector reads: `[0.1,-0.2,…]`.
pub fn literal(v: &[f32]) -> String {
    let mut s = String::with_capacity(v.len() * 10 + 2);
    s.push('[');
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        // Half precision keeps about three digits.
        s.push_str(&format!("{x:.5}"));
    }
    s.push(']');
    s
}

/// A model name as SQL literal (names are checked when configuring; quotes are doubled anyway).
fn quoted(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The name of the index for a space and model.
pub fn index_name(space: Space, model: &str, dim: u32) -> String {
    use sha2::Digest;
    let h = sha2::Sha256::digest(format!("{model}\0{dim}").as_bytes());
    let tag: String = h[..6].iter().map(|b| format!("{b:02x}")).collect();
    format!("ai_vec_{}_{tag}", space.as_str())
}

/// Creates the index for a space and model unless it is there and valid (an interrupted
/// `CONCURRENTLY` leaves an invalid one behind, which is made again).
pub async fn ensure_index(
    db: &PgPool,
    space: Space,
    model: &str,
    dim: u32,
) -> Result<(), sqlx::Error> {
    let name = index_name(space, model, dim);
    let valid: Option<bool> = sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_class c JOIN pg_index i ON i.indexrelid = c.oid
          WHERE c.relname = $1 AND c.relkind = 'i'",
    )
    .bind(&name)
    .fetch_optional(db)
    .await?;
    match valid {
        Some(true) => return Ok(()),
        Some(false) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP INDEX CONCURRENTLY IF EXISTS {name}"
            )))
            .execute(db)
            .await?;
        }
        None => {}
    }
    let filter = format!(
        "space = {} AND model = {}",
        quoted(space.as_str()),
        quoted(model)
    );
    let expr = if space.quantized() {
        format!("(binary_quantize(vec)::bit({dim})) bit_hamming_ops")
    } else {
        format!("(vec::halfvec({dim})) halfvec_cosine_ops")
    };
    tracing::info!(index = %name, model, dim, "KI: Vektorindex wird angelegt");
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {name} ON ai_vectors USING hnsw ({expr}) WHERE {filter}"
    )))
    .execute(db)
    .await?;
    Ok(())
}

/// A piece of a content to store.
pub struct Piece {
    pub start: i32,
    pub len: i32,
    pub vec: Vec<f32>,
}

/// Replaces a content's vectors of one source in one space (within the caller's transaction).
pub async fn replace(
    tx: &mut PgConnection,
    hash: &[u8],
    space: Space,
    model: &str,
    source: &str,
    pieces: &[Piece],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM ai_vectors WHERE content_hash = $1 AND space = $2 AND source = $3")
        .bind(hash)
        .bind(space.as_str())
        .bind(source)
        .execute(&mut *tx)
        .await?;
    if pieces.is_empty() {
        return Ok(());
    }
    let starts: Vec<i32> = pieces.iter().map(|p| p.start).collect();
    let lens: Vec<i32> = pieces.iter().map(|p| p.len).collect();
    let vecs: Vec<String> = pieces.iter().map(|p| literal(&p.vec)).collect();
    sqlx::query(
        "INSERT INTO ai_vectors (content_hash, space, model, source, start, len, vec)
         SELECT $1, $2, $3, $4, s, l, v::halfvec
           FROM unnest($5::int[], $6::int[], $7::text[]) AS t(s, l, v)",
    )
    .bind(hash)
    .bind(space.as_str())
    .bind(model)
    .bind(source)
    .bind(&starts)
    .bind(&lens)
    .bind(&vecs)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// A piece close to a query.
#[derive(sqlx::FromRow, Debug, Clone)]
pub struct Near {
    pub content_hash: Vec<u8>,
    pub source: String,
    pub start: i32,
    pub len: i32,
    /// Cosine distance (0: same direction, 2: opposite).
    pub dist: f64,
}

/// The query for [`nearest`] (`$1`: the query vector as text) and the `hnsw.ef_search` it needs.
pub fn nearest_sql(space: Space, model: &str, dim: u32, limit: usize) -> (String, usize) {
    let filter = format!(
        "space = {} AND model = {}",
        quoted(space.as_str()),
        quoted(model)
    );
    let q = format!("$1::text::halfvec({dim})");
    if space.quantized() {
        let candidates = (limit * OVERSAMPLE).max(40);
        let sql = format!(
            "SELECT content_hash, source, start, len, dist FROM (
                SELECT content_hash, source, start, len,
                       (vec::halfvec({dim}) <=> {q})::float8 AS dist
                  FROM (SELECT content_hash, source, start, len, vec FROM ai_vectors
                         WHERE {filter}
                         ORDER BY binary_quantize(vec)::bit({dim}) <~> binary_quantize({q})
                         LIMIT {candidates}) c
             ) r ORDER BY dist LIMIT {limit}"
        );
        // The index finds at most `ef_search` neighbours.
        (sql, candidates.clamp(40, 1000))
    } else {
        let sql = format!(
            "SELECT content_hash, source, start, len, (vec::halfvec({dim}) <=> {q})::float8 AS dist
               FROM ai_vectors WHERE {filter}
              ORDER BY vec::halfvec({dim}) <=> {q} LIMIT {limit}"
        );
        (sql, limit.clamp(40, 1000))
    }
}

/// The `limit` pieces of a space and model closest to `query` (best first).
pub async fn nearest(
    db: &PgPool,
    space: Space,
    model: &str,
    dim: u32,
    query: &[f32],
    limit: usize,
) -> Result<Vec<Near>, sqlx::Error> {
    let (sql, ef) = nearest_sql(space, model, dim, limit);
    let mut tx = db.begin().await?;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "SET LOCAL hnsw.ef_search = {ef}"
    )))
    .execute(&mut *tx)
    .await?;
    let rows = sqlx::query_as::<_, Near>(sqlx::AssertSqlSafe(sql))
        .bind(literal(query))
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_length_and_text_form() {
        let mut v = vec![3.0, 4.0];
        assert!(normalize(&mut v));
        assert_eq!(literal(&v), "[0.60000,0.80000]");
        assert!(!normalize(&mut [0.0, 0.0]));
        assert!(!normalize(&mut [f32::NAN, 1.0]));
    }

    #[test]
    fn index_names() {
        let a = index_name(Space::Cloud, "qwen3-embedding-8b", 1024);
        assert!(a.starts_with("ai_vec_cloud_") && a.len() == "ai_vec_cloud_".len() + 12);
        assert_ne!(a, index_name(Space::Cloud, "qwen3-embedding-8b", 512));
        assert_ne!(a, index_name(Space::Local, "qwen3-embedding-8b", 1024));
        assert_eq!(quoted("a'b"), "'a''b'");
    }
}

//! Search by meaning (PLAN 6.3, 6.4): the query is embedded per space with that space's model
//! (vectors of different models cannot be compared), and each space answers with its closest
//! contents. The lists are merged with the lexical one by rank only (see `api::search`).
//!
//! A space that does not answer in time is left out; the lexical search always answers.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use super::{Space, vectors};
use crate::state::AppState;

/// How long a query may take to be embedded (the NAS for "Nur lokal", the provider otherwise).
pub const QUERY_TIME: Duration = Duration::from_millis(800);
/// Pieces looked at per space, and contents kept.
const PIECES: usize = 150;
const HITS: usize = 50;
/// Query vectors kept (paging, the same query typed again).
const CACHED: usize = 500;

/// How far a hit may be (cosine distance) for a model family, until measured with real data.
/// Models spread their similarities differently: e5 puts everything close together, CLIP
/// compares pictures with texts and stays far even for good matches.
pub fn default_max_distance(model: &str) -> f64 {
    let m = model.to_ascii_lowercase();
    if m.contains("clip") {
        0.78
    } else if m.contains("e5") {
        0.22
    } else if m.contains("qwen3-embedding") {
        0.55
    } else {
        0.5
    }
}

/// A content close to the query, with the piece that matched.
#[derive(Clone, Debug)]
pub struct Hit {
    pub hash: Vec<u8>,
    /// `text`, `description` or `image`.
    pub source: String,
    pub start: i32,
    pub len: i32,
    pub dist: f64,
}

/// The answer of one space, best first.
#[derive(Clone, Debug)]
pub struct List {
    pub space: Space,
    pub hits: Vec<Hit>,
}

/// The lists of all spaces that have results and answer in time.
pub async fn lists(st: &AppState, q: &str) -> Vec<List> {
    let (a, b, c) = tokio::join!(
        one(st, Space::Cloud, q),
        one(st, Space::Local, q),
        one(st, Space::Clip, q)
    );
    [a, b, c].into_iter().flatten().collect()
}

fn max_distance(st: &AppState, space: Space) -> f64 {
    let ai = &st.cfg.ai;
    match space {
        Space::Cloud => ai.cloud.as_ref().map_or(0.0, |c| c.max_distance),
        Space::Local => ai.local.as_ref().map_or(0.0, |l| l.max_distance),
        Space::Clip => ai.local.as_ref().map_or(0.0, |l| l.clip_max_distance),
    }
}

async fn one(st: &AppState, space: Space, q: &str) -> Option<List> {
    let (model, dim) = st.ai.model(space)?;
    // Without results in this space, the query goes nowhere.
    let any: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM ai_done WHERE space = $1)")
        .bind(space.as_str())
        .fetch_one(&st.db)
        .await
        .ok()?;
    if !any {
        return None;
    }
    let vector = query_vector(st, space, q).await?;
    let near = match vectors::nearest(&st.db, space, model, dim, &vector, PIECES).await {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, space = space.as_str(), "KI: Vektorsuche fehlgeschlagen");
            return None;
        }
    };
    let max = max_distance(st, space);
    let mut seen = HashSet::new();
    let hits = near
        .into_iter()
        .filter(|n| n.dist <= max)
        .filter(|n| seen.insert(n.content_hash.clone()))
        .take(HITS)
        .map(|n| Hit {
            hash: n.content_hash,
            source: n.source,
            start: n.start,
            len: n.len,
            dist: n.dist,
        })
        .collect();
    Some(List { space, hits })
}

/// The query's vector in a space (from the cache, else embedded within [`QUERY_TIME`]).
async fn query_vector(st: &AppState, space: Space, q: &str) -> Option<Vec<f32>> {
    let key = (space, q.to_owned());
    if let Some(v) = st.ai.queries.lock().expect("mutex").get(&key) {
        return Some(v.clone());
    }
    let ai = &st.ai;
    let answer = match space {
        Space::Cloud => {
            let cloud = ai.cloud.as_ref()?;
            let r = cloud.embed.query(q, QUERY_TIME).await;
            if let Ok(e) = &r {
                // A query costs next to nothing; it is counted all the same.
                let _ =
                    super::record_usage(st, "query", e.tokens, 0, cloud.embed_cost(e.tokens)).await;
            }
            r
        }
        Space::Local => ai.local.as_ref()?.embed.query(q, QUERY_TIME).await,
        Space::Clip => ai.local.as_ref()?.clip_query(q, QUERY_TIME).await,
    };
    let vector = match answer {
        Ok(mut e) => e.vectors.pop()?,
        Err(e) => {
            tracing::debug!(error = %e, space = space.as_str(), "KI: Anfrage nicht eingebettet");
            return None;
        }
    };
    let mut cache = st.ai.queries.lock().expect("mutex");
    if cache.len() >= CACHED {
        cache.clear();
    }
    cache.insert(key, vector.clone());
    Some(vector)
}

/// Query vectors by space and text.
pub(crate) type Cache = std::sync::Mutex<HashMap<(Space, String), Vec<f32>>>;

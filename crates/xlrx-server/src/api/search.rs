//! Search (PLAN 6.5): `GET /api/search?q=…` with filters, counts per kind and text snippets;
//! `GET /api/search/suggest?q=…` for names while typing.

use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::files::{self, NodeInfo};
use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::access;
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::search::query::{self, Parsed};
use crate::search::{Found, Part, Request, Search};
use crate::state::AppState;

/// A search waits this long for the index to take in changes made just before (your own upload
/// or rename shows up in the results).
const CATCH_UP_WAIT: Duration = Duration::from_secs(2);
/// …but not while the index is still being built: then it answers right away.
const CATCH_UP_MAX: i64 = 5_000;
/// Text looked at for a snippet (characters from the beginning).
const SNIPPET_SOURCE: i32 = 100_000;

#[derive(Deserialize)]
pub struct SearchQuery {
    pub q: String,
    /// Only below this folder.
    pub folder: Option<i64>,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

#[derive(Serialize, Default)]
pub struct SearchResult {
    pub total: usize,
    pub hits: Vec<Hit>,
    /// Hits per kind (`typ:` value), without the kind filter of the query.
    pub facets: Vec<Facet>,
    /// Nothing matched exactly; these are similar spellings.
    pub fuzzy: bool,
    /// Changes the index has not taken in yet (it is being built or catching up).
    pub pending: i64,
}

#[derive(Serialize)]
pub struct Hit {
    #[serde(flatten)]
    pub node: NodeInfo,
    /// The folder it is in, e.g. "Meine Ablage/Belege".
    pub folder: String,
    /// Where the words occur in the text, if they do.
    pub snippet: Option<Vec<Part>>,
}

#[derive(Serialize)]
pub struct Facet {
    pub typ: &'static str,
    pub n: u64,
}

fn engine(st: &AppState) -> ApiResult<&Search> {
    st.search
        .get()
        .ok_or_else(|| ApiError::Unavailable("Die Suche ist nicht eingerichtet.".into()))
}

/// Runs search work on a blocking thread.
async fn blocking<T, F>(st: &AppState, f: F) -> ApiResult<T>
where
    T: Send + 'static,
    F: FnOnce(&Search) -> Result<T, String> + Send + 'static,
{
    let st = st.clone();
    tokio::task::spawn_blocking(move || f(engine(&st).map_err(|e| format!("{e:?}"))?))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(ApiError::Internal)
}

pub async fn search(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<SearchResult>> {
    let engine = engine(&st)?;
    let parsed = query::parse(&q.q).map_err(ApiError::BadRequest)?;
    let pending = catch_up(&st, engine).await?;
    if parsed.is_empty() {
        return Ok(Json(SearchResult {
            pending,
            ..Default::default()
        }));
    }
    let scope = access::scope(&st.db, me.id).await?;
    let within = match q.folder {
        Some(id) => {
            let (node, _) = files::visible(&st, &me, id).await?;
            if !node.is_dir() {
                return Err(ApiError::bad("Nur in Ordnern kann gesucht werden."));
            }
            Some(id)
        }
        None => None,
    };
    let folders = if parsed.folders.is_empty() {
        None
    } else {
        Some(folder_ids(&st, me.id, &parsed.folders).await?)
    };
    let not_folders = folder_ids(&st, me.id, &parsed.not_folders).await?;
    let request = Request {
        parsed,
        roots: scope.roots.clone(),
        shared: scope.shared.clone(),
        within,
        folders,
        not_folders,
        offset: q.offset.unwrap_or(0).min(10_000),
        limit: q.limit.unwrap_or(30).clamp(1, 100),
        fuzzy: false,
    };
    let (mut request, mut found) = run(&st, request).await?;
    // Nothing at all: perhaps a typo. Try similar spellings once.
    let mut fuzzy = false;
    if found.as_ref().is_some_and(|f| f.total == 0)
        && !request.parsed.clauses.is_empty()
        && request.offset == 0
    {
        request.fuzzy = true;
        (request, found) = run(&st, request).await?;
        fuzzy = found.as_ref().is_some_and(|f| f.total > 0);
    }
    let Some(found) = found else {
        return Ok(Json(SearchResult {
            pending,
            ..Default::default()
        }));
    };

    // Defense in depth: every hit is checked against the database again (live, and the person
    // has a role on it now).
    let rows = checked(&st, me.id, &found.ids).await?;
    let folders = files::folder_names(&st, &scope, &rows).await?;

    let texts = if request.parsed.clauses.is_empty() {
        vec![None; rows.len()]
    } else {
        texts(&st, &rows).await?
    };
    let total = found.total;
    let facets = found
        .facets
        .iter()
        .map(|(c, n)| Facet {
            typ: c.key(),
            n: *n,
        })
        .collect();
    let snippets = blocking(&st, move |s| Ok(s.snippets(&found, &texts))).await?;
    let hits = rows
        .iter()
        .zip(snippets)
        .map(|(n, snippet)| Hit {
            node: NodeInfo::from(n),
            folder: folders.get(&n.id).cloned().unwrap_or_default(),
            snippet,
        })
        .collect();
    Ok(Json(SearchResult {
        total,
        hits,
        facets,
        fuzzy,
        pending,
    }))
}

async fn run(st: &AppState, request: Request) -> ApiResult<(Request, Option<Found>)> {
    blocking(st, move |s| {
        let found = s.run(&request)?;
        Ok((request, found))
    })
    .await
}

/// Lets the index take in what changed just before the search (nodes and texts). Returns how
/// many journal entries it is still behind.
async fn catch_up(st: &AppState, engine: &Search) -> ApiResult<i64> {
    let (journal, text): (i64, i64) = sqlx::query_as(
        "SELECT (SELECT coalesce(max(seq), 0) FROM journal),
                (SELECT coalesce(max(seq), 0) FROM content_text)",
    )
    .fetch_one(&st.db)
    .await?;
    let mut at = engine.cursor();
    if (journal > at.journal || text > at.text) && journal - at.journal <= CATCH_UP_MAX {
        at = engine.caught_up(journal, text, CATCH_UP_WAIT).await;
    }
    Ok((journal - at.journal).max(0))
}

/// Live nodes with these ids that the person may see, in the given order.
async fn checked(st: &AppState, user_id: i64, ids: &[i64]) -> ApiResult<Vec<NodeRow>> {
    let visible = access::visible_ids(&st.db, user_id, ids).await?;
    let rows: Vec<NodeRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {NODE_COLS} FROM nodes WHERE id = ANY($1) AND deleted_at IS NULL"
    )))
    .bind(ids)
    .fetch_all(&st.db)
    .await?;
    let mut by_id: HashMap<i64, NodeRow> = rows
        .into_iter()
        .filter(|n| visible.contains(&n.id))
        .map(|n| (n.id, n))
        .collect();
    Ok(ids.iter().filter_map(|i| by_id.remove(i)).collect())
}

/// Folders with one of these names that the person can see (exact, else names starting so).
async fn folder_ids(st: &AppState, user_id: i64, names: &[String]) -> ApiResult<Vec<i64>> {
    if names.is_empty() {
        return Ok(vec![]);
    }
    let folded: Vec<String> = names.iter().map(|n| db::fold(n)).collect();
    let find = |pattern: bool| {
        let folded = folded.clone();
        async move {
            let ids: Vec<i64> = if pattern {
                let patterns: Vec<String> = folded
                    .iter()
                    .map(|f| {
                        let escaped = f
                            .replace('\\', "\\\\")
                            .replace('%', "\\%")
                            .replace('_', "\\_");
                        format!("{escaped}%")
                    })
                    .collect();
                sqlx::query_scalar(
                    "SELECT id FROM nodes
                      WHERE kind = 'dir' AND deleted_at IS NULL AND parent_id IS NOT NULL
                        AND name_folded LIKE ANY($1)
                      LIMIT 5000",
                )
                .bind(&patterns)
                .fetch_all(&st.db)
                .await?
            } else {
                sqlx::query_scalar(
                    "SELECT id FROM nodes
                      WHERE kind = 'dir' AND deleted_at IS NULL AND parent_id IS NOT NULL
                        AND name_folded = ANY($1)
                      LIMIT 5000",
                )
                .bind(&folded)
                .fetch_all(&st.db)
                .await?
            };
            let visible = access::visible_ids(&st.db, user_id, &ids).await?;
            Ok::<_, ApiError>(
                ids.into_iter()
                    .filter(|i| visible.contains(i))
                    .collect::<Vec<i64>>(),
            )
        }
    };
    let exact = find(false).await?;
    if !exact.is_empty() {
        return Ok(exact);
    }
    find(true).await
}

/// The beginning of the extracted text of each file, with its language.
async fn texts(
    st: &AppState,
    rows: &[NodeRow],
) -> ApiResult<Vec<Option<(Option<String>, String)>>> {
    let hashes: Vec<Vec<u8>> = rows.iter().filter_map(|n| n.content_hash.clone()).collect();
    if hashes.is_empty() {
        return Ok(vec![None; rows.len()]);
    }
    let found: HashMap<Vec<u8>, (Option<String>, String)> =
        sqlx::query_as::<_, (Vec<u8>, Option<String>, String)>(
            "SELECT hash, lang, left(text, $2) FROM content_text WHERE hash = ANY($1)",
        )
        .bind(&hashes)
        .bind(SNIPPET_SOURCE)
        .fetch_all(&st.db)
        .await?
        .into_iter()
        .map(|(h, lang, text)| (h, (lang, text)))
        .collect();
    Ok(rows
        .iter()
        .map(|n| n.content_hash.as_ref().and_then(|h| found.get(h).cloned()))
        .collect())
}

#[derive(Deserialize)]
pub struct SuggestQuery {
    pub q: String,
    pub limit: Option<usize>,
}

#[derive(Serialize)]
pub struct Suggestion {
    #[serde(flatten)]
    pub node: NodeInfo,
    pub folder: String,
    /// The name, split into the matched word beginnings and the rest.
    pub parts: Vec<Part>,
}

/// Names starting like the words typed so far (no full text; filters are ignored).
pub async fn suggest(
    State(st): State<AppState>,
    me: CurrentUser,
    Query(q): Query<SuggestQuery>,
) -> ApiResult<Json<Vec<Suggestion>>> {
    engine(&st)?;
    // Half-typed filters ("nach:20") are normal while typing: no error, just no suggestions.
    let Ok(Parsed { clauses, .. }) = query::parse(&q.q) else {
        return Ok(Json(vec![]));
    };
    if clauses.is_empty() {
        return Ok(Json(vec![]));
    }
    let scope = access::scope(&st.db, me.id).await?;
    let limit = q.limit.unwrap_or(8).clamp(1, 20);
    let (ids, clauses) = {
        let scope = scope.clone();
        blocking(&st, move |s| {
            let ids = s.suggest(&clauses, &scope.roots, &scope.shared, limit)?;
            Ok((ids, clauses))
        })
        .await?
    };
    let rows = checked(&st, me.id, &ids).await?;
    let folders = files::folder_names(&st, &scope, &rows).await?;
    let s = engine(&st)?;
    Ok(Json(
        rows.iter()
            .map(|n| Suggestion {
                node: NodeInfo::from(n),
                folder: folders.get(&n.id).cloned().unwrap_or_default(),
                parts: s.name_parts(&n.name, &clauses),
            })
            .collect(),
    ))
}

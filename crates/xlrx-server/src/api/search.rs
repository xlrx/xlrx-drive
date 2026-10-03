//! Search (PLAN 6.5): `GET /api/search?q=…` with filters, counts per kind and text snippets;
//! `GET /api/search/suggest?q=…` for names while typing.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use super::files::{self, NodeInfo};
use crate::ai::{self, Space};
use crate::auth::session::CurrentUser;
use crate::error::{ApiError, ApiResult};
use crate::files::access;
use crate::files::db::{self, NODE_COLS, NodeRow};
use crate::search::query::{self, Parsed};
use crate::search::schema::Category;
use crate::search::{Found, Part, Request, Search, Sources};
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
    /// Found by meaning only (no word matches): `bedeutung` (a text) or `bild` (a picture).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<&'static str>,
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
    let offset = q.offset.unwrap_or(0).min(10_000);
    let limit = q.limit.unwrap_or(30).clamp(1, 100);
    // With search by meaning, the lexical list is merged from its start (PLAN 6.4).
    let meaning = if st.ai.configured() {
        meaning_of(&parsed)
    } else {
        None
    };
    let request = Request {
        parsed,
        roots: scope.roots.clone(),
        shared: scope.shared.clone(),
        within,
        folders,
        not_folders,
        offset: if meaning.is_some() { 0 } else { offset },
        limit: if meaning.is_some() {
            offset + limit + MERGE_EXTRA
        } else {
            limit
        },
        fuzzy: false,
    };
    let by_meaning = async {
        match &meaning {
            Some(text) => ai::search::lists(&st, text).await,
            None => Vec::new(),
        }
    };
    let (lexical, lists) = tokio::join!(run(&st, request), by_meaning);
    let (mut request, mut found) = lexical?;
    let merged = merge_meaning(&st, &request, lists).await?;
    // Nothing at all: perhaps a typo. Try similar spellings once.
    let mut fuzzy = false;
    if found.as_ref().is_some_and(|f| f.total == 0)
        && merged.ids.is_empty()
        && !request.parsed.clauses.is_empty()
        && offset == 0
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
    let (page, total, facets) = if meaning.is_some() {
        let order = merged.order(&found.ids);
        let page: Vec<i64> = order.into_iter().skip(offset).take(limit).collect();
        let only = merged.ids.difference(&merged.lexical).count();
        let mut facets: HashMap<Category, u64> = found.facets.iter().copied().collect();
        for (c, n) in &merged.facets {
            *facets.entry(*c).or_default() += n;
        }
        let mut facets: Vec<(Category, u64)> = facets.into_iter().collect();
        facets.sort();
        (page, found.total + only, facets)
    } else {
        (found.ids.clone(), found.total, found.facets.clone())
    };

    // Defense in depth: every hit is checked against the database again (live, and the person
    // has a role on it now).
    let rows = checked(&st, me.id, &page).await?;
    let folders = files::folder_names(&st, &scope, &rows).await?;

    let texts = if request.parsed.clauses.is_empty() {
        vec![Default::default(); rows.len()]
    } else {
        texts(&st, &rows).await?
    };
    let facets = facets
        .iter()
        .map(|(c, n)| Facet {
            typ: c.key(),
            n: *n,
        })
        .collect();
    let snippets = blocking(&st, move |s| Ok(s.snippets(&found, &texts))).await?;
    let pieces = merged.pieces(&st, &rows).await?;
    let hits = rows
        .iter()
        .zip(snippets)
        .map(|(n, snippet)| {
            let only_meaning = merged.ids.contains(&n.id) && !merged.lexical.contains(&n.id);
            let (by, piece) = match merged.best.get(&n.id) {
                Some(b) if only_meaning => (Some(b.how()), pieces.get(&n.id).cloned()),
                _ => (None, None),
            };
            Hit {
                node: NodeInfo::from(n),
                folder: folders.get(&n.id).cloned().unwrap_or_default(),
                // Where the words are; for a hit found by meaning, the piece that matched.
                snippet: snippet.or_else(|| piece.map(|text| vec![Part { text, hit: false }])),
                by,
            }
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

/// Lexical hits looked at beyond the requested page when merging with hits by meaning.
const MERGE_EXTRA: usize = 50;
/// Reciprocal Rank Fusion: a list's first place counts 1/(K+1) (PLAN 6.4).
const RRF_K: f64 = 60.0;
/// Characters of a matching piece shown for a hit found by meaning.
const PIECE_CHARS: i32 = 220;

/// The words to search by meaning: those of the query (phrases included, without exclusions
/// and filters), if there is something to understand.
fn meaning_of(p: &Parsed) -> Option<String> {
    let text = p
        .clauses
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    (text.chars().filter(|c| c.is_alphanumeric()).count() >= 3).then_some(text)
}

/// Hits found by meaning, checked against the request.
#[derive(Default)]
struct Merged {
    /// Per space: node ids, best first (duplicates of a content share its place).
    lists: Vec<Vec<Vec<i64>>>,
    /// Those matching the request.
    ids: HashSet<i64>,
    /// Of those, the ones the words match too.
    lexical: HashSet<i64>,
    /// Counts per kind of those only meaning found.
    facets: Vec<(Category, u64)>,
    /// The best matching piece per node.
    best: HashMap<i64, (Space, ai::search::Hit)>,
}

trait How {
    fn how(&self) -> &'static str;
}

impl How for (Space, ai::search::Hit) {
    /// How the hit was found: by what a text means or by what a picture shows.
    fn how(&self) -> &'static str {
        match self.1.source.as_str() {
            "text" => "bedeutung",
            _ => "bild",
        }
    }
}

impl Merged {
    /// All hits in merged order: Reciprocal Rank Fusion of the lexical list and the lists by
    /// meaning; ties keep the lexical order.
    fn order(&self, lexical: &[i64]) -> Vec<i64> {
        let mut score: HashMap<i64, f64> = HashMap::new();
        let mut first: HashMap<i64, usize> = HashMap::new();
        for (rank, id) in lexical.iter().enumerate() {
            *score.entry(*id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
            first.entry(*id).or_insert(rank);
        }
        for list in &self.lists {
            let mut rank = 0;
            for ids in list {
                let ids: Vec<i64> = ids
                    .iter()
                    .copied()
                    .filter(|i| self.ids.contains(i))
                    .collect();
                if ids.is_empty() {
                    continue;
                }
                for id in ids {
                    *score.entry(id).or_default() += 1.0 / (RRF_K + rank as f64 + 1.0);
                    first.entry(id).or_insert(usize::MAX);
                }
                rank += 1;
            }
        }
        let mut all: Vec<(i64, f64, usize)> = score
            .into_iter()
            .map(|(id, s)| (id, s, first[&id]))
            .collect();
        all.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));
        all.into_iter().map(|(id, _, _)| id).collect()
    }

    /// The matching piece of text (or picture description) per node found by meaning.
    async fn pieces(&self, st: &AppState, rows: &[NodeRow]) -> ApiResult<HashMap<i64, String>> {
        let mut out = HashMap::new();
        for n in rows {
            let Some((_, hit)) = self.best.get(&n.id) else {
                continue;
            };
            let text: Option<String> = match hit.source.as_str() {
                "text" => sqlx::query_scalar(
                    "SELECT substr(text, $2 + 1, least($3, $4)) FROM content_text WHERE hash = $1",
                )
                .bind(&hit.hash)
                .bind(hit.start)
                .bind(hit.len)
                .bind(PIECE_CHARS)
                .fetch_optional(&st.db)
                .await?,
                "description" => {
                    sqlx::query_scalar("SELECT description FROM ai_vision WHERE content_hash = $1")
                        .bind(&hit.hash)
                        .fetch_optional(&st.db)
                        .await?
                }
                _ => None,
            };
            if let Some(t) = text {
                let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
                if !t.is_empty() {
                    out.insert(n.id, t);
                }
            }
        }
        Ok(out)
    }
}

/// Turns the lists by meaning (contents) into nodes and checks them against the request.
async fn merge_meaning(
    st: &AppState,
    request: &Request,
    lists: Vec<ai::search::List>,
) -> ApiResult<Merged> {
    let mut merged = Merged::default();
    let hashes: Vec<Vec<u8>> = lists
        .iter()
        .flat_map(|l| l.hits.iter().map(|h| h.hash.clone()))
        .collect();
    if hashes.is_empty() {
        return Ok(merged);
    }
    let nodes: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT id, content_hash FROM nodes
          WHERE content_hash = ANY($1) AND deleted_at IS NULL AND kind = 'file'",
    )
    .bind(&hashes)
    .fetch_all(&st.db)
    .await?;
    let mut by_hash: HashMap<Vec<u8>, Vec<i64>> = HashMap::new();
    for (id, h) in nodes {
        by_hash.entry(h).or_default().push(id);
    }
    let candidates: Vec<i64> = by_hash.values().flatten().copied().collect();
    let checked = {
        let request = request.clone();
        blocking(st, move |s| s.check(&request, &candidates)).await?
    };
    for l in lists {
        let mut list = Vec::new();
        for hit in l.hits {
            let ids = by_hash.get(&hit.hash).cloned().unwrap_or_default();
            for id in &ids {
                if checked.ids.contains(id) {
                    // The best piece: lists come in order, the first is kept… unless a closer
                    // one comes from another space.
                    let better = merged.best.get(id).is_none_or(|(_, b)| hit.dist < b.dist);
                    if better {
                        merged.best.insert(*id, (l.space, hit.clone()));
                    }
                }
            }
            list.push(ids);
        }
        merged.lists.push(list);
    }
    merged.ids = checked.ids;
    merged.lexical = checked.lexical;
    merged.facets = checked.facets;
    Ok(merged)
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
async fn texts(st: &AppState, rows: &[NodeRow]) -> ApiResult<Vec<Sources>> {
    let hashes: Vec<Vec<u8>> = rows.iter().filter_map(|n| n.content_hash.clone()).collect();
    if hashes.is_empty() {
        return Ok(vec![Default::default(); rows.len()]);
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
    // What the AI saw in pictures.
    let seen: HashMap<Vec<u8>, String> = sqlx::query_as::<_, (Vec<u8>, String)>(
        "SELECT content_hash, concat_ws(E'\\n', description, array_to_string(tags, ', '), text_in_image)
           FROM ai_vision WHERE content_hash = ANY($1)",
    )
    .bind(&hashes)
    .fetch_all(&st.db)
    .await?
    .into_iter()
    .collect();
    Ok(rows
        .iter()
        .map(|n| Sources {
            text: n.content_hash.as_ref().and_then(|h| found.get(h).cloned()),
            seen: n.content_hash.as_ref().and_then(|h| seen.get(h).cloned()),
        })
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

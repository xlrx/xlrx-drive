//! Full-text search (PLAN 6): a Tantivy index in the state directory, fed from the journal and
//! from the texts extracted from file content.
//!
//! The index holds derived data only. It records how far it got in its own commits, so after a
//! crash it continues where its last commit ended; a missing or damaged index, or one built with
//! another schema, is rebuilt from the database. Rights are checked twice: the index only looks
//! at the roots a person may read, and every hit is checked again in Postgres.

pub mod indexer;
pub mod query;
pub mod schema;
pub mod text;

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::columnar::ColumnValues;
use tantivy::query::{
    BooleanQuery, BoostQuery, ConstScoreQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    RangeQuery, TermQuery, TermSetQuery,
};
use tantivy::schema::{Field, IndexRecordOption};
use tantivy::snippet::SnippetGenerator;
use tantivy::{
    DocAddress, DocId, Index, IndexReader, Order, ReloadPolicy, Score, Searcher, SegmentOrdinal,
    SegmentReader, Term,
};
use tokio::sync::{Notify, watch};

use crate::state::AppState;
use query::{Clause, Parsed};
use schema::{Category, Fields};

/// State shared between the indexer and the searches.
pub struct Shared {
    pub(crate) index: Index,
    pub(crate) reader: IndexReader,
    pub(crate) fields: Fields,
    /// How far the searchable index got.
    pub(crate) indexed: watch::Sender<indexer::Cursor>,
    /// Asks the indexer to look for changes now.
    pub(crate) wake: Notify,
}

pub struct Search {
    shared: Arc<Shared>,
    stop: watch::Sender<bool>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Opens (or rebuilds) the index and starts keeping it up to date.
pub async fn start(st: &AppState) -> Result<(), String> {
    let state_dir = st
        .cfg
        .state_dir
        .clone()
        .ok_or("Kein Zustandsverzeichnis für den Suchindex")?;
    let dir = state_dir.join("index");
    let (index, cursor, writer) = tokio::task::spawn_blocking(move || {
        let (index, cursor) = indexer::open(&dir)?;
        let writer = indexer::writer(&index)?;
        Ok::<_, String>((index, cursor, writer))
    })
    .await
    .map_err(|e| e.to_string())??;
    let reader = index
        .reader_builder()
        .reload_policy(ReloadPolicy::Manual)
        .try_into()
        .map_err(|e| format!("Suchindex: {e}"))?;
    let (_, fields) = schema::build();
    let live = crate::files::live::subscribe(st)
        .await
        .map_err(|e| format!("Journal: {e:?}"))?;
    let shared = Arc::new(Shared {
        index,
        reader,
        fields,
        indexed: watch::Sender::new(cursor),
        wake: Notify::new(),
    });
    let (stop, stop_rx) = watch::channel(false);
    let task = tokio::spawn(indexer::run(
        st.db.clone(),
        shared.clone(),
        writer,
        cursor,
        live,
        stop_rx,
    ));
    st.search
        .set(Search {
            shared,
            stop,
            task: tokio::sync::Mutex::new(Some(task)),
        })
        .map_err(|_| "Die Suche läuft schon".to_string())
}

/// What a search looks for, with rights and folder names already resolved.
pub struct Request {
    pub parsed: Parsed,
    /// Roots the person may read.
    pub roots: Vec<i64>,
    /// Only below this directory.
    pub within: Option<i64>,
    /// Directories named with `in:` (any of them); `None` without `in:`.
    pub folders: Option<Vec<i64>>,
    pub not_folders: Vec<i64>,
    pub offset: usize,
    pub limit: usize,
    /// Also similar spellings (when nothing matched exactly).
    pub fuzzy: bool,
}

pub struct Found {
    /// Node ids, best first.
    pub ids: Vec<i64>,
    pub total: usize,
    /// Hits per category, without the `typ:` filter (so the other kinds show their counts).
    pub facets: Vec<(Category, u64)>,
    pub searcher: Searcher,
    pub query: Box<dyn Query>,
}

struct Built {
    query: Box<dyn Query>,
    /// The same without the `typ:` filter (for the counts per kind).
    without_types: Box<dyn Query>,
    /// Has words to rank by; otherwise only filters (newest first).
    scored: bool,
}

/// A piece of a snippet or name; `hit` marks what matched.
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Part {
    pub text: String,
    pub hit: bool,
}

/// Field weights: the name counts most, its exact words more than word stems.
const NAME_BOOST: Score = 4.0;
const NAME_STEM_BOOST: Score = 3.0;
const PREFIX_BOOST: Score = 2.0;
const TEXT_BOOST: Score = 1.0;
/// Most characters of a snippet.
const SNIPPET_CHARS: usize = 200;

impl Search {
    /// Journal sequence the searchable index reflects.
    pub fn indexed(&self) -> i64 {
        self.shared.indexed.borrow().journal
    }

    /// Waits (at most `wait`) until the index reflects the journal up to `journal` and the texts
    /// up to `text`. Returns how far it got.
    pub async fn caught_up(&self, journal: i64, text: i64, wait: Duration) -> indexer::Cursor {
        self.shared.wake.notify_one();
        let mut rx = self.shared.indexed.subscribe();
        let _ = tokio::time::timeout(
            wait,
            rx.wait_for(|c| c.journal >= journal && c.text >= text),
        )
        .await;
        *self.shared.indexed.borrow()
    }

    /// How far the searchable index got.
    pub fn cursor(&self) -> indexer::Cursor {
        *self.shared.indexed.borrow()
    }

    /// Asks the indexer to look for changes now (e.g. after storing a text).
    pub fn wake(&self) {
        self.shared.wake.notify_one();
    }

    /// Stops the indexer (it releases the index).
    pub async fn stop(&self) {
        let _ = self.stop.send(true);
        if let Some(task) = self.task.lock().await.take() {
            let _ = task.await;
        }
    }

    fn tokens(&self, field: Field, text: &str) -> Vec<String> {
        let Ok(mut analyzer) = self.shared.index.tokenizer_for_field(field) else {
            return Vec::new();
        };
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    /// A word or phrase in any of the name and text fields.
    fn clause(&self, c: &Clause, fuzzy: bool) -> Option<Box<dyn Query>> {
        let f = &self.shared.fields;
        let mut should: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (field, boost) in [
            (f.name, NAME_BOOST),
            (f.name_de, NAME_STEM_BOOST),
            (f.name_en, NAME_STEM_BOOST),
            (f.text, TEXT_BOOST),
            (f.text_de, TEXT_BOOST),
            (f.text_en, TEXT_BOOST),
        ] {
            let tokens = self.tokens(field, &c.text);
            let q: Box<dyn Query> = match tokens.as_slice() {
                [] => continue,
                [t] => term_query(field, t, fuzzy && !c.quoted),
                _ => Box::new(PhraseQuery::new(
                    tokens
                        .iter()
                        .map(|t| Term::from_field_text(field, t))
                        .collect(),
                )),
            };
            should.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
        }
        if !c.quoted {
            // While typing: beginnings of words in the name ("Rech" finds "Rechnung.pdf").
            let all: Vec<(Occur, Box<dyn Query>)> = self
                .tokens(f.name, &c.text)
                .iter()
                .map(|t| {
                    let prefix: String = t.chars().take(schema::PREFIX_MAX).collect();
                    let q: Box<dyn Query> = Box::new(TermQuery::new(
                        Term::from_field_text(f.name_pre, &prefix),
                        IndexRecordOption::Basic,
                    ));
                    (Occur::Must, q)
                })
                .collect();
            if !all.is_empty() {
                should.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(BooleanQuery::new(all)),
                        PREFIX_BOOST,
                    )),
                ));
            }
        }
        (!should.is_empty()).then(|| Box::new(BooleanQuery::new(should)) as Box<dyn Query>)
    }

    /// The query for a request. `None` if it cannot match anything (no readable root, words that
    /// are all punctuation).
    fn build(&self, r: &Request) -> Option<Built> {
        let f = &self.shared.fields;
        if r.roots.is_empty() || r.folders.as_ref().is_some_and(Vec::is_empty) {
            return None;
        }
        let mut words = Vec::new();
        for c in &r.parsed.clauses {
            // A clause without searchable characters ("!!!") is left out rather than failing.
            if let Some(q) = self.clause(c, r.fuzzy) {
                words.push(q);
            }
        }
        if words.is_empty() && !r.parsed.has_filters() {
            return None;
        }
        let scored = !words.is_empty();
        let ids = |field: Field, ids: &[i64]| -> Box<dyn Query> {
            Box::new(TermSetQuery::new(
                ids.iter().map(|&i| Term::from_field_u64(field, i as u64)),
            ))
        };
        let mut filters: Vec<Box<dyn Query>> = vec![ids(f.root, &r.roots)];
        if let Some(w) = r.within {
            filters.push(ids(f.anc, &[w]));
        }
        if let Some(folders) = &r.folders {
            filters.push(ids(f.anc, folders));
        }
        if r.parsed.after.is_some() || r.parsed.before.is_some() {
            let from = r.parsed.after.unwrap_or(i64::MIN);
            let to = r.parsed.before.unwrap_or(i64::MAX);
            filters.push(Box::new(RangeQuery::new(
                Bound::Included(Term::from_field_i64(f.mtime, from)),
                Bound::Excluded(Term::from_field_i64(f.mtime, to)),
            )));
        }
        let mut not: Vec<Box<dyn Query>> = Vec::new();
        for c in &r.parsed.excluded {
            // Exact words only: "-Entwurf" should not also drop "Entwurfsplan".
            let exact = Clause {
                text: c.text.clone(),
                quoted: true,
            };
            not.extend(self.clause(&exact, false));
        }
        if !r.parsed.not_types.is_empty() {
            not.push(self.types(&r.parsed.not_types));
        }
        if !r.not_folders.is_empty() {
            not.push(ids(f.anc, &r.not_folders));
        }
        let types = (!r.parsed.types.is_empty()).then(|| self.types(&r.parsed.types));

        let compose = |with_types: bool| -> Box<dyn Query> {
            let mut all: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            all.extend(words.iter().map(|q| (Occur::Must, q.box_clone())));
            // Filters decide what matches, not how well.
            let constant = |q: &dyn Query| -> Box<dyn Query> {
                Box::new(ConstScoreQuery::new(q.box_clone(), 0.0))
            };
            all.extend(filters.iter().map(|q| (Occur::Must, constant(q.as_ref()))));
            if with_types && let Some(t) = &types {
                all.push((Occur::Must, constant(t.as_ref())));
            }
            all.extend(not.iter().map(|q| (Occur::MustNot, q.box_clone())));
            Box::new(BooleanQuery::new(all))
        };
        Some(Built {
            query: compose(true),
            without_types: compose(false),
            scored,
        })
    }

    fn types(&self, values: &[String]) -> Box<dyn Query> {
        let f = &self.shared.fields;
        let any: Vec<(Occur, Box<dyn Query>)> = values
            .iter()
            .map(|v| {
                let term = match Category::parse(v) {
                    Some(c) => Term::from_field_u64(f.cat, c.code()),
                    None => Term::from_field_text(f.ext, v),
                };
                let q: Box<dyn Query> = Box::new(TermQuery::new(term, IndexRecordOption::Basic));
                (Occur::Should, q)
            })
            .collect();
        Box::new(BooleanQuery::new(any))
    }

    /// Runs a search (blocking: call from a blocking thread).
    pub fn run(&self, r: &Request) -> Result<Option<Found>, String> {
        let Some(Built {
            query,
            without_types,
            scored,
        }) = self.build(r)
        else {
            return Ok(None);
        };
        let searcher = self.shared.reader.searcher();
        let top = TopDocs::with_limit(r.limit.max(1)).and_offset(r.offset);
        let err = |e: tantivy::TantivyError| format!("Suche: {e}");
        let (addrs, total, counts): (Vec<DocAddress>, usize, Counts) = if scored {
            let now = time::OffsetDateTime::now_utc().unix_timestamp();
            let top = top.tweak_score(move |seg: &SegmentReader| {
                let mtime = seg
                    .fast_fields()
                    .i64("mtime")
                    .ok()
                    .map(|c| c.first_or_default_col(0));
                move |doc: DocId, score: Score| score * freshness(now, mtime.as_deref(), doc)
            });
            let (hits, total, counts) = searcher
                .search(query.as_ref(), &(top, Count, CategoryCounts))
                .map_err(err)?;
            (hits.into_iter().map(|h| h.1).collect(), total, counts)
        } else {
            // Only filters: newest first.
            let top = top.order_by_fast_field::<i64>("mtime", Order::Desc);
            let (hits, total, counts) = searcher
                .search(query.as_ref(), &(top, Count, CategoryCounts))
                .map_err(err)?;
            (hits.into_iter().map(|h| h.1).collect(), total, counts)
        };
        let counts = if r.parsed.types.is_empty() {
            counts
        } else {
            searcher
                .search(without_types.as_ref(), &CategoryCounts)
                .map_err(err)?
        };
        let mut id_columns = HashMap::new();
        let mut ids = Vec::with_capacity(addrs.len());
        for a in addrs {
            let col = match id_columns.entry(a.segment_ord) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(
                    searcher
                        .segment_reader(a.segment_ord)
                        .fast_fields()
                        .u64("id")
                        .map_err(err)?,
                ),
            };
            if let Some(id) = col.first(a.doc_id) {
                ids.push(id as i64);
            }
        }
        let facets = Category::ALL
            .iter()
            .zip(counts)
            .filter(|(_, n)| *n > 0)
            .map(|(c, n)| (*c, n))
            .collect();
        Ok(Some(Found {
            ids,
            total,
            facets,
            searcher,
            query,
        }))
    }

    /// Snippets of the given texts (language, text) showing where the query matched (blocking).
    pub fn snippets(
        &self,
        found: &Found,
        texts: &[Option<(Option<String>, String)>],
    ) -> Vec<Option<Vec<Part>>> {
        let f = &self.shared.fields;
        let mut generators: HashMap<Field, Option<SnippetGenerator>> = HashMap::new();
        let mut snippet = |field: Field, text: &str| -> Option<Vec<Part>> {
            let generator = generators
                .entry(field)
                .or_insert_with(|| {
                    SnippetGenerator::create(&found.searcher, found.query.as_ref(), field)
                        .ok()
                        .map(|mut g| {
                            g.set_max_num_chars(SNIPPET_CHARS);
                            g
                        })
                })
                .as_ref()?;
            let s = generator.snippet(text);
            if s.highlighted().is_empty() {
                return None;
            }
            Some(parts(s.fragment(), s.highlighted()))
        };
        texts
            .iter()
            .map(|t| {
                let (lang, text) = t.as_ref()?;
                match lang.as_deref() {
                    Some("deu") => snippet(f.text_de, text),
                    Some("eng") => snippet(f.text_en, text),
                    Some(_) => snippet(f.text, text),
                    None => snippet(f.text_de, text).or_else(|| snippet(f.text_en, text)),
                }
            })
            .collect()
    }

    /// Files and folders whose names start like the words typed (suggestions while typing;
    /// blocking). Newer ones first among equally good matches.
    pub fn suggest(
        &self,
        words: &[Clause],
        roots: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, String> {
        let f = &self.shared.fields;
        if roots.is_empty() || words.is_empty() {
            return Ok(vec![]);
        }
        // Names only: every word must start a word of the name.
        let mut all: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for c in words {
            for t in self.tokens(f.name, &c.text) {
                let prefix: String = t.chars().take(schema::PREFIX_MAX).collect();
                let exact: Box<dyn Query> = Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(f.name, &t),
                        IndexRecordOption::WithFreqs,
                    )),
                    NAME_BOOST,
                ));
                let starts: Box<dyn Query> = Box::new(TermQuery::new(
                    Term::from_field_text(f.name_pre, &prefix),
                    IndexRecordOption::WithFreqs,
                ));
                all.push((
                    Occur::Must,
                    Box::new(BooleanQuery::new(vec![
                        (Occur::Must, starts),
                        (Occur::Should, exact),
                    ])),
                ));
            }
        }
        if all.is_empty() {
            return Ok(vec![]);
        }
        all.push((
            Occur::Must,
            Box::new(ConstScoreQuery::new(
                Box::new(TermSetQuery::new(
                    roots
                        .iter()
                        .map(|&r| Term::from_field_u64(f.root, r as u64)),
                )),
                0.0,
            )),
        ));
        let query = BooleanQuery::new(all);
        let searcher = self.shared.reader.searcher();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let top = TopDocs::with_limit(limit.max(1)).tweak_score(move |seg: &SegmentReader| {
            let mtime = seg
                .fast_fields()
                .i64("mtime")
                .ok()
                .map(|c| c.first_or_default_col(0));
            move |doc: DocId, score: Score| score * freshness(now, mtime.as_deref(), doc)
        });
        let hits = searcher
            .search(&query, &top)
            .map_err(|e| format!("Suche: {e}"))?;
        let mut ids = Vec::with_capacity(hits.len());
        for (_, a) in hits {
            let col = searcher
                .segment_reader(a.segment_ord)
                .fast_fields()
                .u64("id")
                .map_err(|e| format!("Suche: {e}"))?;
            if let Some(id) = col.first(a.doc_id) {
                ids.push(id as i64);
            }
        }
        Ok(ids)
    }

    /// The name split into what the typed words matched (word beginnings) and the rest.
    pub fn name_parts(&self, name: &str, words: &[Clause]) -> Vec<Part> {
        let f = &self.shared.fields;
        let typed: Vec<String> = words
            .iter()
            .flat_map(|c| self.tokens(f.name, &c.text))
            .collect();
        let Ok(mut analyzer) = self.shared.index.tokenizer_for_field(f.name) else {
            return vec![Part {
                text: name.into(),
                hit: false,
            }];
        };
        let mut ranges = Vec::new();
        let mut stream = analyzer.token_stream(name);
        while stream.advance() {
            let t = stream.token();
            let Some(len) = typed
                .iter()
                .filter(|w| t.text.starts_with(w.as_str()))
                .map(|w| w.chars().count())
                .max()
            else {
                continue;
            };
            // Folding can change lengths ("ß" → "ss"); never reach past the word.
            let word = &name[t.offset_from..t.offset_to];
            let end = word.char_indices().nth(len).map_or(word.len(), |(i, _)| i);
            ranges.push(t.offset_from..t.offset_from + end);
        }
        parts(name, &ranges)
    }
}

fn term_query(field: Field, token: &str, fuzzy: bool) -> Box<dyn Query> {
    let term = Term::from_field_text(field, token);
    let n = token.chars().count();
    if fuzzy && n >= 4 {
        Box::new(FuzzyTermQuery::new(term, if n >= 8 { 2 } else { 1 }, true))
    } else {
        Box::new(TermQuery::new(term, IndexRecordOption::WithFreqs))
    }
}

/// A little more weight for recent changes: ×1.3 today, ×1.15 after half a year.
fn freshness(now: i64, mtime: Option<&dyn ColumnValues<i64>>, doc: DocId) -> Score {
    let Some(col) = mtime else { return 1.0 };
    let days = (now - col.get_val(doc)).max(0) as Score / 86_400.0;
    1.0 + 0.3 / (1.0 + days / 180.0)
}

/// Splits text at the (byte) ranges that matched. Line breaks and runs of spaces from extracted
/// text become single spaces, which never count as part of a match.
fn parts(text: &str, ranges: &[std::ops::Range<usize>]) -> Vec<Part> {
    let mut ranges: Vec<_> = ranges
        .iter()
        .filter(|r| {
            r.start < r.end
                && r.end <= text.len()
                && text.is_char_boundary(r.start)
                && text.is_char_boundary(r.end)
        })
        .cloned()
        .collect();
    ranges.sort_by_key(|r| r.start);
    let mut pieces: Vec<(&str, bool)> = Vec::new();
    let mut at = 0;
    for r in ranges {
        let start = r.start.max(at);
        if start >= r.end || !text.is_char_boundary(start) {
            continue;
        }
        pieces.push((&text[at..start], false));
        pieces.push((&text[start..r.end], true));
        at = r.end;
    }
    pieces.push((&text[at..], false));

    let mut out: Vec<Part> = Vec::new();
    let mut emit = |c: char, hit: bool| match out.last_mut() {
        Some(last) if last.hit == hit => last.text.push(c),
        _ => out.push(Part {
            text: c.to_string(),
            hit,
        }),
    };
    let (mut started, mut space) = (false, false);
    for (piece, hit) in pieces {
        for c in piece.chars() {
            if c.is_whitespace() {
                space = started;
                continue;
            }
            if space {
                emit(' ', false);
                space = false;
            }
            emit(c, hit);
            started = true;
        }
    }
    out
}

type Counts = [u64; Category::ALL.len()];

/// Counts hits per category.
struct CategoryCounts;

struct SegmentCounts {
    cat: Arc<dyn ColumnValues<u64>>,
    counts: Counts,
}

impl Collector for CategoryCounts {
    type Fruit = Counts;
    type Child = SegmentCounts;

    fn for_segment(
        &self,
        _: SegmentOrdinal,
        segment: &SegmentReader,
    ) -> tantivy::Result<SegmentCounts> {
        Ok(SegmentCounts {
            cat: segment
                .fast_fields()
                .u64("cat")?
                .first_or_default_col(u64::MAX),
            counts: [0; Category::ALL.len()],
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<Counts>) -> tantivy::Result<Counts> {
        let mut sum = [0; Category::ALL.len()];
        for f in fruits {
            for (s, n) in sum.iter_mut().zip(f) {
                *s += n;
            }
        }
        Ok(sum)
    }
}

impl SegmentCollector for SegmentCounts {
    type Fruit = Counts;

    fn collect(&mut self, doc: DocId, _: Score) {
        if let Some(n) = usize::try_from(self.cat.get_val(doc))
            .ok()
            .and_then(|c| self.counts.get_mut(c))
        {
            *n += 1;
        }
    }

    fn harvest(self) -> Counts {
        self.counts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str, hit: bool) -> Part {
        Part {
            text: text.into(),
            hit,
        }
    }

    #[test]
    fn parts_keep_words_apart() {
        let t = "Die  Rechnung\nfür die Heizung ";
        assert_eq!(
            parts(t, &[5..13, 23..30]),
            [
                p("Die ", false),
                p("Rechnung", true),
                p(" für die ", false),
                p("Heizung", true)
            ]
        );
        assert_eq!(parts("abc", &[]), [p("abc", false)]);
        // Overlapping and out-of-range ranges do no harm.
        assert_eq!(
            parts("abcdef", &[0..3, 2..4, 5..99]),
            [p("abcd", true), p("ef", false)]
        );
    }
}

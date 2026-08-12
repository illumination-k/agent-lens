//! `analyze search` — rank functions by BM25F relevance to a query.
//!
//! The unit of retrieval is the function, which is the whole reason
//! this exists next to `grep`. A line-oriented search answers "where
//! does this string occur", leaves the caller to rebuild the enclosing
//! definitions, and returns its matches unranked — so a two-hundred-hit
//! query costs an agent its context window rather than answering the
//! question. Here every hit is a definition with a span, a relevance
//! score, the line inside it that matched best, and a per-term
//! breakdown of *why* it ranked.
//!
//! Scoring lives in [`lens_domain::search`]; this module is the corpus
//! projection and the report. Each function becomes a five-field
//! document — name, file path, signature, doc comment, body text — and
//! BM25F weights the fields separately, so a function *named* after the
//! query outranks one that merely mentions it.
//!
//! Two ranking modes:
//!
//! * `bm25` (default) — pure textual relevance.
//! * `graph` — relevance scaled by a call-graph importance prior,
//!   `1 + ln(1 + fan_in)`. This is the query `grep` cannot express:
//!   "the *load-bearing* function matching this", not merely "a
//!   function matching this". It is a re-rank of the top BM25
//!   candidates (the pool is [`GRAPH_POOL_FACTOR`]× the reported
//!   limit), so a hub with very low textual relevance stays outside the
//!   pool and is not surfaced — the mode reorders good matches, it does
//!   not find new ones.
//!
//! No index is persisted. The corpus is parsed per run, exactly like
//! every other analyzer here, so results are never stale — which is the
//! property that makes an index worth using at all next to a tool that
//! always reads the current tree.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;

use lens_domain::FunctionDef;
use lens_domain::search::{
    Bm25Options, FuzzyOptions, IndexOptions, SearchDocument, SearchHit, SearchIndex, tokenize,
};
use rayon::prelude::*;
use serde::Serialize;

use super::call_graph::{CallGraphBuilder, delegate_call_graph_builders};
use super::runner::render_report;
use super::similarity::FunctionSelection;
use super::{
    AnalyzeRoots, AnalyzerError, OutputFormat, SourceFile, collect_source_files, read_source,
};

const SCHEMA_VERSION: u32 = 1;

/// Default cap on reported hits.
pub const DEFAULT_SEARCH_LIMIT: usize = 20;

/// How many BM25 candidates per reported hit the graph re-rank considers.
/// Re-ranking only the reported window would let the importance prior
/// shuffle rows without ever promoting one; a wider pool is what makes
/// the mode do anything, and a bounded one is what keeps it a re-rank
/// rather than a second retrieval.
pub const GRAPH_POOL_FACTOR: usize = 5;

/// Longest snippet line emitted, in characters. Long enough for a
/// signature, short enough that a full result list stays readable in an
/// agent's context.
const SNIPPET_MAX_CHARS: usize = 160;

/// Per-hit term breakdown entries rendered in markdown. JSON carries
/// them all.
const MD_TERM_LIMIT: usize = 5;

/// When a query term is expanded to nearby vocabulary via character
/// n-grams.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum FuzzyMode {
    /// Literal terms only.
    Off,
    /// Expand only the query terms the corpus never spells — the
    /// half-remembered identifier, the typo, and (because a script
    /// without word boundaries tokenizes to one long term) a substring
    /// of a longer token.
    #[default]
    Missing,
    /// Also expand terms that already matched, which buys morphological
    /// reach (`retry` finding `retries`) at the cost of some noise.
    Always,
}

impl FuzzyMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Missing => "missing",
            Self::Always => "always",
        }
    }

    fn options(self) -> Option<FuzzyOptions> {
        match self {
            Self::Off => None,
            Self::Missing => Some(FuzzyOptions::default()),
            Self::Always => Some(FuzzyOptions {
                expand_known_terms: true,
                ..FuzzyOptions::default()
            }),
        }
    }
}

/// What the reported score means.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize, Serialize,
)]
#[serde(rename_all = "lowercase")]
pub enum RankMode {
    /// Textual relevance alone.
    #[default]
    Bm25,
    /// Relevance scaled by call-graph importance.
    Graph,
}

impl RankMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Bm25 => "bm25",
            Self::Graph => "graph",
        }
    }
}

/// `analyze search` flags, and the `[profile.<name>.search]` table.
///
/// Like `graph-query`, this tool has a required key — a search with no
/// query has no meaning — so the type is written out rather than
/// generated by `analyzer_options!`, and [`crate::config::Config`]
/// rejects a profile that lists the tool without this table.
#[derive(Debug, Clone, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct SearchOptions {
    /// What to search for. Free text: it is tokenized the same way the
    /// corpus is, so `parse_diff_range`, `parseDiffRange` and `parse
    /// diff range` are the same query.
    #[arg(long)]
    pub query: String,
    /// Cap the result list (default 20).
    #[arg(long)]
    pub limit: Option<usize>,
    /// When to expand a query term to nearby vocabulary via character
    /// n-grams (default missing).
    #[arg(long, value_enum)]
    pub fuzzy: Option<FuzzyMode>,
    /// Whether to scale relevance by call-graph importance (default
    /// bm25).
    #[arg(long, value_enum)]
    pub rank: Option<RankMode>,
}

/// Analyzer entry point for `analyze search`.
///
/// Holds a [`CallGraphBuilder`] rather than the usual `FilterConfig`:
/// the `graph` rank mode needs the graph anyway, and sharing the
/// builder means the searched corpus and the graph that re-ranks it are
/// walked through exactly the same filter instead of two that can
/// drift.
#[derive(Debug, Clone)]
pub struct SearchAnalyzer {
    builder: CallGraphBuilder,
    selection: FunctionSelection,
    query: String,
    limit: Option<usize>,
    fuzzy: FuzzyMode,
    rank: RankMode,
}

impl SearchAnalyzer {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            builder: CallGraphBuilder::new(),
            selection: FunctionSelection::All,
            query: query.into(),
            limit: None,
            fuzzy: FuzzyMode::default(),
            rank: RankMode::default(),
        }
    }

    /// Build from a whole [`SearchOptions`] group. Constructs rather
    /// than configures, because `query` is required.
    pub fn from_options(opts: SearchOptions) -> Self {
        Self::new(opts.query)
            .with_limit(opts.limit)
            .with_fuzzy(opts.fuzzy.unwrap_or_default())
            .with_rank(opts.rank.unwrap_or_default())
    }

    pub fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    pub fn with_fuzzy(mut self, fuzzy: FuzzyMode) -> Self {
        self.fuzzy = fuzzy;
        self
    }

    pub fn with_rank(mut self, rank: RankMode) -> Self {
        self.rank = rank;
        self
    }

    /// Restrict which functions are indexed, independently of the
    /// path-level filter — the same two granularities similarity uses,
    /// so a `#[cfg(test)]` function inside a production file obeys
    /// `--exclude-tests`.
    pub fn with_function_selection(mut self, selection: FunctionSelection) -> Self {
        self.selection = selection;
        self
    }

    delegate_call_graph_builders! {
        builder,
        only_tests,
        exclude_tests,
    }

    /// Walk `roots`, index them, and report the best matches for the
    /// query in `format`.
    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        let query_terms = tokenize(&self.query);
        // A query that tokenizes to nothing cannot match anything, and
        // an empty report would read as "nothing here matches" — the
        // opposite of what happened.
        if query_terms.is_empty() {
            return Err(AnalyzerError::EmptySearchQuery {
                query: self.query.clone(),
            });
        }

        let corpus = self.collect_corpus(&roots)?;
        let index = SearchIndex::build(
            &corpus.documents,
            IndexOptions {
                bm25: Bm25Options::default(),
                fuzzy: self.fuzzy.options(),
            },
        );
        let limit = self.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
        // In `graph` mode the prior re-orders, so the pool scored has
        // to be wider than the window reported — and the report says
        // how wide, because that width is exactly what bounds the
        // mode's reach.
        let candidate_pool = match self.rank {
            RankMode::Bm25 => None,
            RankMode::Graph => Some(limit.saturating_mul(GRAPH_POOL_FACTOR)),
        };
        let hits = index.search(&self.query, candidate_pool.unwrap_or(limit));
        let ranked = self.rank_hits(&roots, &corpus, hits, limit)?;

        let report = Report {
            schema_version: SCHEMA_VERSION,
            root: roots.display(),
            query: self.query.clone(),
            query_terms: dedup_preserving_order(query_terms),
            rank: self.rank.as_str(),
            fuzzy: self.fuzzy.as_str(),
            scanned_file_count: corpus.scanned_file_count,
            indexed_function_count: index.document_count(),
            indexed_term_count: index.term_count(),
            limit,
            candidate_pool,
            hit_count: ranked.len(),
            hits: ranked.iter().map(|r| HitView::build(&corpus, r)).collect(),
        };
        render_report(&report, format, || format_markdown(&report))
    }

    /// Apply the ranking mode and truncate to `limit`.
    ///
    /// In `bm25` mode this is the identity on an already-ordered list.
    /// In `graph` mode every candidate is scaled by its importance
    /// prior and the pool is re-sorted, so a strongly-called function
    /// can overtake a slightly more literal match.
    fn rank_hits(
        &self,
        roots: &AnalyzeRoots,
        corpus: &Corpus,
        hits: Vec<SearchHit>,
        limit: usize,
    ) -> Result<Vec<RankedHit>, AnalyzerError> {
        let mut ranked: Vec<RankedHit> = match self.rank {
            RankMode::Bm25 => hits
                .into_iter()
                .map(|hit| RankedHit {
                    score: hit.score,
                    fan_in: None,
                    hit,
                })
                .collect(),
            RankMode::Graph => {
                let graph = self.builder.build(roots)?;
                let fan_in_by_span: HashMap<(&str, usize), usize> = graph
                    .nodes
                    .iter()
                    .map(|node| ((node.file.as_str(), node.start_line), node.weights.fan_in))
                    .collect();
                hits.into_iter()
                    .map(|hit| {
                        let record = &corpus.records[hit.document];
                        // A function the graph never saw — an
                        // extension the graph does not cover, or a span
                        // the two extractors disagree on — is reported
                        // as fan-in 0 rather than dropped: absent
                        // evidence must not read as "nothing calls it,
                        // and also this hit does not exist".
                        let fan_in = fan_in_by_span
                            .get(&(record.file.as_str(), record.start_line))
                            .copied()
                            .unwrap_or(0);
                        RankedHit {
                            score: hit.score * importance_prior(fan_in),
                            fan_in: Some(fan_in),
                            hit,
                        }
                    })
                    .collect()
            }
        };
        ranked.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.hit.document.cmp(&b.hit.document))
        });
        ranked.truncate(limit);
        Ok(ranked)
    }

    /// Walk `roots` through the graph builder's own collection filter
    /// and project every selected function into a searchable document.
    fn collect_corpus(&self, roots: &AnalyzeRoots) -> Result<Corpus, AnalyzerError> {
        let filter = self.builder.collection_filter().compile(roots.base())?;
        let files = collect_source_files(roots, &filter)?;
        let per_file: Vec<Vec<Entry>> = files
            .par_iter()
            .map(|file| self.collect_file(file, filter.is_test_path(&file.path)))
            .collect::<Result<_, _>>()?;

        let mut documents = Vec::new();
        let mut records = Vec::new();
        for entry in per_file.into_iter().flatten() {
            documents.push(entry.document);
            records.push(entry.record);
        }
        Ok(Corpus {
            documents,
            records,
            scanned_file_count: files.len(),
        })
    }

    fn collect_file(
        &self,
        file: &SourceFile,
        path_is_test: bool,
    ) -> Result<Vec<Entry>, AnalyzerError> {
        let (lang, source) = read_source(&file.path)?;
        let mut parser = lang.create_language_parser();
        let functions = parser
            .extract_functions(&source)
            .map_err(|err| AnalyzerError::Parse(Box::new(err)))?;
        let lines: Vec<&str> = source.lines().collect();
        Ok(functions
            .into_iter()
            .filter_map(|def| {
                let is_test = def.is_test || path_is_test;
                if !self.selection.includes(is_test) {
                    return None;
                }
                let body = slice_lines(&lines, def.start_line, def.end_line);
                Some(Entry {
                    document: SearchDocument {
                        name: def.name.clone(),
                        path: file.display_path.clone(),
                        signature: signature_text(&def),
                        doc: def.doc.clone().unwrap_or_default(),
                        body: body.clone(),
                    },
                    record: FunctionRecord {
                        file: file.display_path.clone(),
                        name: def.name,
                        start_line: def.start_line,
                        end_line: def.end_line,
                        is_test,
                        body,
                    },
                })
            })
            .collect())
    }
}

/// One corpus entry before it is split into the index's view and the
/// report's view of the same function.
#[derive(Debug)]
struct Entry {
    document: SearchDocument,
    record: FunctionRecord,
}

/// A hit with the ranking mode applied.
#[derive(Debug)]
struct RankedHit {
    hit: SearchHit,
    /// Distinct resolved callers, or `None` in `bm25` mode where the
    /// graph was never built.
    fan_in: Option<usize>,
    /// The score the list is ordered by.
    score: f64,
}

/// Call-graph importance as a multiplier on relevance.
///
/// Logarithmic so the prior orders hits without overwhelming them: the
/// difference between one caller and ten matters, the difference
/// between a hundred and a thousand barely does.
fn importance_prior(fan_in: usize) -> f64 {
    1.0 + (1.0 + fan_in as f64).ln()
}

/// What the report needs about an indexed function.
#[derive(Debug)]
struct FunctionRecord {
    file: String,
    name: String,
    start_line: usize,
    end_line: usize,
    is_test: bool,
    /// The definition's source text, kept so a hit can quote the line
    /// inside it that matched best.
    body: String,
}

#[derive(Debug)]
struct Corpus {
    documents: Vec<SearchDocument>,
    records: Vec<FunctionRecord>,
    scanned_file_count: usize,
}

/// Join the 1-based inclusive line range `[start, end]` of `lines`.
/// Out-of-range endpoints clamp rather than panic: adapters report
/// spans, and a span that disagrees with the file should degrade to a
/// shorter body, not abort the run.
fn slice_lines(lines: &[&str], start: usize, end: usize) -> String {
    let start = start.saturating_sub(1).min(lines.len());
    let end = end.min(lines.len()).max(start);
    lines[start..end].join("\n")
}

/// The searchable text of a signature: parameter names and type paths,
/// return types, generics, and the implemented trait. Deliberately not
/// the source spelling — this field exists so a query naming a type
/// (`DiffScope`) reaches the functions that take or return one.
fn signature_text(def: &FunctionDef) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if let Some(implements) = &def.implements {
        parts.push(implements);
    }
    if let Some(signature) = &def.signature {
        for group in [
            &signature.parameter_names,
            &signature.parameter_type_paths,
            &signature.return_type_paths,
            &signature.generics,
        ] {
            parts.extend(group.iter().map(String::as_str));
        }
    }
    parts.join(" ")
}

fn dedup_preserving_order(terms: Vec<String>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    terms
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// The distinct indexed terms a hit matched, used to pick its snippet.
fn matched_terms(hit: &SearchHit) -> BTreeSet<&str> {
    hit.terms
        .iter()
        .map(|term| term.matched_term.as_str())
        .collect()
}

#[derive(Debug, Serialize)]
struct Report {
    schema_version: u32,
    root: String,
    query: String,
    /// The query as the index saw it, after tokenization.
    query_terms: Vec<String>,
    rank: &'static str,
    fuzzy: &'static str,
    scanned_file_count: usize,
    indexed_function_count: usize,
    indexed_term_count: usize,
    limit: usize,
    /// How many relevance candidates the ranking mode considered.
    /// Absent in `bm25` mode, where that is the limit by construction.
    #[serde(skip_serializing_if = "Option::is_none")]
    candidate_pool: Option<usize>,
    hit_count: usize,
    hits: Vec<HitView>,
}

#[derive(Debug, Serialize)]
struct HitView {
    file: String,
    name: String,
    start_line: usize,
    end_line: usize,
    is_test: bool,
    /// Final ranking score — equal to `relevance` unless the graph rank
    /// mode scaled it.
    score: f64,
    /// Textual BM25F relevance on its own.
    relevance: f64,
    /// Distinct resolved callers, when the graph rank mode ran.
    #[serde(skip_serializing_if = "Option::is_none")]
    fan_in: Option<usize>,
    snippet: Snippet,
    matched: Vec<MatchView>,
}

#[derive(Debug, Serialize)]
struct Snippet {
    line: usize,
    text: String,
}

#[derive(Debug, Serialize)]
struct MatchView {
    query_term: String,
    /// The indexed term that matched. Differs from `query_term` only
    /// for an n-gram expansion.
    term: String,
    /// Trigram similarity, present only for an expansion.
    #[serde(skip_serializing_if = "Option::is_none")]
    similarity: Option<f64>,
    score: f64,
    fields: Vec<&'static str>,
}

/// Round to three decimals so a report is stable to compare and cheap
/// to read; ranking has already happened on the full-precision values.
fn round(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

impl HitView {
    fn build(corpus: &Corpus, ranked: &RankedHit) -> Self {
        let hit = &ranked.hit;
        let record = &corpus.records[hit.document];
        Self {
            file: record.file.clone(),
            name: record.name.clone(),
            start_line: record.start_line,
            end_line: record.end_line,
            is_test: record.is_test,
            score: round(ranked.score),
            relevance: round(hit.score),
            fan_in: ranked.fan_in,
            snippet: best_snippet(record, &matched_terms(hit)),
            matched: hit
                .terms
                .iter()
                .map(|term| MatchView {
                    query_term: term.query_term.clone(),
                    term: term.matched_term.clone(),
                    similarity: term.is_expansion().then(|| round(term.similarity)),
                    score: round(term.score),
                    fields: term.fields.iter().map(|field| field.as_str()).collect(),
                })
                .collect(),
        }
    }
}

/// The line inside the definition carrying the most distinct matched
/// terms, or its first line when the match came from fields the body
/// does not contain (path, doc). Ties go to the earliest line, which is
/// usually the declaration.
fn best_snippet(record: &FunctionRecord, matched: &BTreeSet<&str>) -> Snippet {
    let mut best = (0usize, 0usize);
    for (offset, line) in record.body.lines().enumerate() {
        let hits = tokenize(line)
            .into_iter()
            .collect::<BTreeSet<String>>()
            .iter()
            .filter(|token| matched.contains(token.as_str()))
            .count();
        if hits > best.1 {
            best = (offset, hits);
        }
    }
    let text = record.body.lines().nth(best.0).unwrap_or_default().trim();
    Snippet {
        line: record.start_line + best.0,
        text: truncate(text),
    }
}

fn truncate(text: &str) -> String {
    if text.chars().count() <= SNIPPET_MAX_CHARS {
        return text.to_owned();
    }
    text.chars().take(SNIPPET_MAX_CHARS).collect::<String>() + "…"
}

fn format_markdown(report: &Report) -> String {
    let mut out = format!(
        "# Search: {} ({} hit(s), rank {}, fuzzy {})\n",
        report.query, report.hit_count, report.rank, report.fuzzy,
    );
    let _ = writeln!(
        out,
        "\n- root: {}\n- corpus: {} function(s) across {} file(s), {} indexed term(s)\n- query terms: {}",
        report.root,
        report.indexed_function_count,
        report.scanned_file_count,
        report.indexed_term_count,
        report.query_terms.join(" "),
    );
    if let Some(candidate_pool) = report.candidate_pool {
        let _ = writeln!(
            out,
            "- graph rank re-orders the top {candidate_pool} relevance candidates; a hub below that cut is not surfaced",
        );
    }
    if report.hits.is_empty() {
        out.push_str("\nNo function matched.\n");
        return out;
    }
    out.push('\n');
    for (rank, hit) in report.hits.iter().enumerate() {
        let _ = writeln!(
            out,
            "{}. {}:{}-{} {}{} score={}{}",
            rank + 1,
            hit.file,
            hit.start_line,
            hit.end_line,
            hit.name,
            if hit.is_test { " [test]" } else { "" },
            hit.score,
            match hit.fan_in {
                Some(fan_in) => format!(" relevance={} fan-in={fan_in}", hit.relevance),
                None => String::new(),
            },
        );
        let _ = writeln!(out, "   L{}: {}", hit.snippet.line, hit.snippet.text);
        let terms: Vec<String> = hit
            .matched
            .iter()
            .take(MD_TERM_LIMIT)
            .map(|m| {
                format!(
                    "{}[{}]={}{}",
                    m.term,
                    m.fields.join(","),
                    m.score,
                    match m.similarity {
                        Some(similarity) => format!("~{}:{similarity}", m.query_term),
                        None => String::new(),
                    },
                )
            })
            .collect();
        let _ = writeln!(out, "   terms: {}", terms.join(" "));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_file;
    use rstest::rstest;
    use serde_json::Value;
    use std::path::Path;
    use tempfile::TempDir;

    fn json(path: &Path, analyzer: SearchAnalyzer) -> Value {
        let report = analyzer.analyze(path, OutputFormat::Json).unwrap();
        serde_json::from_str(&report).unwrap()
    }

    fn names(report: &Value) -> Vec<String> {
        report["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["name"].as_str().unwrap().to_owned())
            .collect()
    }

    /// One Rust file with a definition whose *name* states the query, a
    /// second that only mentions it in prose, and a `#[cfg(test)]`
    /// function that also mentions it.
    fn fixture() -> TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            r#"
/// Parse a git revision range.
pub fn parse_diff_range(range: &str) -> Result<String, String> {
    Ok(range.to_owned())
}

/// Unrelated helper that happens to mention the diff range in prose.
pub fn helper() -> usize {
    let diff = 1;
    let range = 2;
    diff + range
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_a_diff_range() {
        let _ = super::parse_diff_range("HEAD~1..HEAD");
    }
}
"#,
        );
        dir
    }

    /// The headline claim: the definition the query names ranks first,
    /// ahead of code that merely mentions the same words.
    #[test]
    fn the_definition_the_query_names_ranks_first() {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new("parse_diff_range"));
        assert_eq!(names(&report)[0], "parse_diff_range", "{report:#}");
        assert_eq!(report["hits"][0]["file"], "src/lib.rs");
        assert_eq!(report["rank"], "bm25");
        assert_eq!(report["query_terms"][0], "parsediffrange");
    }

    /// Spelling convention is not part of the query: the tokenizer
    /// indexes the joined form, so all three spellings are one query.
    #[rstest]
    #[case("parse_diff_range")]
    #[case("parseDiffRange")]
    #[case("parse diff range")]
    fn identifier_spelling_does_not_change_the_answer(#[case] query: &str) {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new(query));
        assert_eq!(names(&report)[0], "parse_diff_range", "{report:#}");
    }

    /// The snippet is the *best-matching* line, not the first one — a
    /// hit whose evidence is buried mid-body has to point at the
    /// evidence, or the reader has to open the file anyway. Ties go to
    /// the earliest line.
    #[test]
    fn the_snippet_points_into_the_body_not_at_the_declaration() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn alpha() -> usize {\n    let a = 1;\n    let sentinel = 2;\n    a + sentinel\n}\n",
        );
        let report = json(dir.path(), SearchAnalyzer::new("sentinel"));
        let hit = &report["hits"][0];
        assert_eq!(hit["start_line"], 1);
        assert_eq!(hit["snippet"]["line"], 3, "{report:#}");
        assert_eq!(hit["snippet"]["text"], "let sentinel = 2;");
    }

    /// A parameter's name and type reach the index through the
    /// signature field, which no other field would carry: the query
    /// term here appears in the declaration only.
    #[test]
    fn a_signature_match_is_reported_as_one() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            "pub fn alpha(needle_param: usize) -> usize {\n    let a = 1;\n    a\n}\n",
        );
        let report = json(dir.path(), SearchAnalyzer::new("needle_param"));
        let fields = report["hits"][0]["matched"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["term"] == "needleparam")
            .expect("the joined parameter name must be indexed")["fields"]
            .as_array()
            .unwrap()
            .clone();
        assert!(fields.contains(&Value::from("signature")), "{report:#}");
    }

    /// The hit quotes the line inside the definition that matched, not
    /// the definition's first line by default — that is what makes a
    /// result actionable without opening the file.
    #[test]
    fn a_hit_quotes_the_best_matching_line_in_its_span() {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new("parse_diff_range"));
        let hit = &report["hits"][0];
        assert_eq!(hit["snippet"]["line"], 3);
        assert_eq!(
            hit["snippet"]["text"],
            "pub fn parse_diff_range(range: &str) -> Result<String, String> {",
        );
        assert!(hit["start_line"].as_u64().unwrap() <= 3);
        assert!(hit["end_line"].as_u64().unwrap() >= 3);
    }

    /// Every hit says why it ranked, per term and per field. Without
    /// that a ranked list is just an ordering the reader has to trust.
    #[test]
    fn a_hit_reports_the_terms_and_fields_behind_its_score() {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new("parse_diff_range"));
        let matched = report["hits"][0]["matched"].as_array().unwrap();
        let joined = matched
            .iter()
            .find(|m| m["term"] == "parsediffrange")
            .expect("the joined identifier term must be reported");
        assert_eq!(joined["query_term"], "parsediffrange");
        assert!(
            joined["similarity"].is_null(),
            "a literal match, not an expansion"
        );
        assert!(
            joined["fields"]
                .as_array()
                .unwrap()
                .contains(&Value::from("name")),
        );
    }

    #[test]
    fn test_functions_are_dropped_by_the_function_selection() {
        let dir = fixture();
        let all = json(dir.path(), SearchAnalyzer::new("diff range"));
        assert!(names(&all).iter().any(|n| n == "parses_a_diff_range"));

        let production = json(
            dir.path(),
            SearchAnalyzer::new("diff range")
                .with_exclude_tests(true)
                .with_function_selection(FunctionSelection::ExcludeTests),
        );
        assert!(
            !names(&production)
                .iter()
                .any(|n| n == "parses_a_diff_range"),
            "{production:#}",
        );
        assert!(names(&production).iter().any(|n| n == "parse_diff_range"));
    }

    /// The mode is named in the report, and `always` actually changes
    /// what is scored: a term the corpus *does* spell gains expansions
    /// that `missing` withholds.
    #[rstest]
    #[case(FuzzyMode::Off, "off")]
    #[case(FuzzyMode::Missing, "missing")]
    #[case(FuzzyMode::Always, "always")]
    fn the_fuzzy_mode_is_named_in_the_report(#[case] mode: FuzzyMode, #[case] expected: &str) {
        let dir = fixture();
        let report = json(
            dir.path(),
            SearchAnalyzer::new("diff range").with_fuzzy(mode),
        );
        assert_eq!(report["fuzzy"], expected);
    }

    #[test]
    fn always_expands_a_term_the_corpus_already_spells() {
        let dir = fixture();
        let expansions = |mode: FuzzyMode| {
            let report = json(dir.path(), SearchAnalyzer::new("range").with_fuzzy(mode));
            report["hits"]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|hit| hit["matched"].as_array().unwrap())
                .filter(|m| !m["similarity"].is_null())
                .count()
        };
        assert_eq!(
            expansions(FuzzyMode::Missing),
            0,
            "`range` is in the corpus"
        );
        assert!(expansions(FuzzyMode::Always) > 0);
    }

    #[test]
    fn limit_caps_the_reported_hits() {
        let dir = fixture();
        let report = json(
            dir.path(),
            SearchAnalyzer::new("diff range").with_limit(Some(1)),
        );
        assert_eq!(report["hit_count"], 1);
        assert_eq!(report["limit"], 1);
        assert_eq!(report["hits"].as_array().unwrap().len(), 1);
    }

    /// A misspelling reaches its definition only through n-gram
    /// expansion, and the report marks the term as expanded rather than
    /// passing it off as a literal match.
    #[test]
    fn a_misspelled_query_reaches_the_definition_and_says_so() {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new("parse_diff_rnge"));
        assert_eq!(names(&report)[0], "parse_diff_range", "{report:#}");
        let expanded = report["hits"][0]["matched"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["term"] == "parsediffrange")
            .expect("the misspelling must expand to the real identifier");
        assert_eq!(expanded["query_term"], "parsediffrnge");
        assert!(expanded["similarity"].as_f64().unwrap() > 0.5);

        // Without expansion the sub-tokens still reach the
        // definition, but the identifier itself contributes nothing —
        // so the evidence is thinner and every reported term is
        // literal.
        let strict = json(
            dir.path(),
            SearchAnalyzer::new("parse_diff_rnge").with_fuzzy(FuzzyMode::Off),
        );
        assert_eq!(names(&strict)[0], "parse_diff_range", "{strict:#}");
        assert!(
            strict["hits"][0]["matched"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["similarity"].is_null()),
            "{strict:#}",
        );
        assert!(
            strict["hits"][0]["score"].as_f64().unwrap()
                < report["hits"][0]["score"].as_f64().unwrap(),
        );
    }

    /// The graph mode's whole point: a function many callers depend on
    /// overtakes a slightly more literal match that nothing calls.
    #[test]
    fn graph_rank_promotes_a_called_function_over_a_leaf() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            "src/lib.rs",
            r#"
pub fn render_report_markdown() -> String {
    let render = "render report";
    let report = "render report";
    format!("{render}{report}")
}

pub fn render_report() -> String {
    String::new()
}

pub fn alpha() -> String {
    render_report()
}

pub fn beta() -> String {
    render_report()
}

pub fn gamma() -> String {
    render_report()
}
"#,
        );

        let relevance = json(dir.path(), SearchAnalyzer::new("render report"));
        assert_eq!(names(&relevance)[0], "render_report_markdown");
        assert!(relevance["hits"][0]["fan_in"].is_null());

        let graph = json(
            dir.path(),
            SearchAnalyzer::new("render report").with_rank(RankMode::Graph),
        );
        assert_eq!(names(&graph)[0], "render_report", "{graph:#}");
        assert_eq!(graph["hits"][0]["fan_in"], 3);
        // The relevance component survives alongside the ranked score,
        // so the promotion is visible rather than implied — and the
        // score is relevance *scaled* by the prior, not offset by it.
        let hit = &graph["hits"][0];
        let (relevance, score) = (
            hit["relevance"].as_f64().unwrap(),
            hit["score"].as_f64().unwrap(),
        );
        assert!(relevance < score);
        let prior = importance_prior(3);
        // Both figures are rounded to three decimals for the report, so
        // the equality holds to within one rounding step on each side.
        assert!(
            (score - relevance * prior).abs() <= 0.0005 * prior + 0.0005,
            "score {score} must be relevance {relevance} scaled by {prior}: {graph:#}",
        );
        assert_eq!(graph["candidate_pool"], 100);
    }

    /// `bm25` mode never builds the graph, so it reports neither a
    /// fan-in nor a candidate pool — the absence is the honest signal
    /// that no importance evidence was gathered.
    #[test]
    fn relevance_mode_reports_no_graph_evidence() {
        let dir = fixture();
        let report = json(dir.path(), SearchAnalyzer::new("parse_diff_range"));
        assert!(report.get("candidate_pool").is_none(), "{report:#}");
        assert!(report["hits"][0]["fan_in"].is_null());
    }

    /// Every adapter feeds the same index, so a query crosses language
    /// boundaries in one corpus.
    #[test]
    fn the_corpus_spans_every_supported_language() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "a.rs", "pub fn retry_backoff() {}\n");
        write_file(dir.path(), "b.ts", "export function retryBackoff() {}\n");
        write_file(
            dir.path(),
            "c.py",
            "def retry_backoff(n):\n    total = n + 1\n    return total\n",
        );
        write_file(
            dir.path(),
            "d.go",
            "package main\n\nfunc RetryBackoff() {}\n",
        );

        let report = json(dir.path(), SearchAnalyzer::new("retry backoff"));
        assert_eq!(report["hit_count"], 4, "{report:#}");
        assert_eq!(report["scanned_file_count"], 4);
    }

    /// An unsearchable query must fail rather than report zero hits,
    /// which would read as "the corpus has nothing like this".
    #[test]
    fn a_query_with_no_searchable_term_is_an_error() {
        let dir = fixture();
        let err = SearchAnalyzer::new("!!! ???")
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(
            matches!(err, AnalyzerError::EmptySearchQuery { .. }),
            "{err:?}",
        );
        assert!(err.to_string().contains("!!! ???"), "{err}");
    }

    #[test]
    fn markdown_carries_the_span_the_snippet_and_the_terms() {
        let dir = fixture();
        let md = SearchAnalyzer::new("parse_diff_range")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.starts_with("# Search: parse_diff_range"), "{md}");
        assert!(md.contains("src/lib.rs:3-5 parse_diff_range"), "{md}");
        assert!(md.contains("   L3: pub fn parse_diff_range"), "{md}");
        assert!(md.contains("parsediffrange[name,body]="), "{md}");
        assert!(
            !md.contains("graph rank re-orders"),
            "the graph caveat belongs to the graph mode only: {md}",
        );
    }

    #[test]
    fn markdown_states_the_graph_modes_candidate_cut() {
        let dir = fixture();
        let md = SearchAnalyzer::new("parse_diff_range")
            .with_rank(RankMode::Graph)
            .with_limit(Some(4))
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.contains("graph rank re-orders the top 20 relevance"),
            "{md}"
        );
    }

    #[test]
    fn markdown_says_so_when_nothing_matched() {
        let dir = fixture();
        let md = SearchAnalyzer::new("zzzzzzzz")
            .analyze(dir.path(), OutputFormat::Md)
            .unwrap();
        assert!(md.contains("No function matched."), "{md}");
    }

    #[rstest]
    // 1-based inclusive, both endpoints kept.
    #[case(2, 3, "b\nc")]
    #[case(1, 1, "a")]
    // Endpoints past the file clamp instead of panicking.
    #[case(3, 99, "c\nd")]
    #[case(99, 99, "")]
    // An inverted span yields nothing rather than reversing.
    #[case(3, 1, "")]
    fn slice_lines_clamps_out_of_range_spans(
        #[case] start: usize,
        #[case] end: usize,
        #[case] expected: &str,
    ) {
        assert_eq!(slice_lines(&["a", "b", "c", "d"], start, end), expected);
    }

    #[rstest]
    #[case(0, 1.0)]
    #[case(1, 1.693)]
    #[case(100, 5.615)]
    fn importance_prior_grows_logarithmically(#[case] fan_in: usize, #[case] expected: f64) {
        assert_eq!(round(importance_prior(fan_in)), expected);
    }

    #[test]
    fn truncate_marks_the_cut() {
        let long = "x".repeat(SNIPPET_MAX_CHARS + 10);
        let cut = truncate(&long);
        assert_eq!(cut.chars().count(), SNIPPET_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(truncate("short"), "short");
    }
}

//! BM25F retrieval over source-derived documents, with a character
//! n-gram fallback for query terms the corpus never spells.
//!
//! The unit of retrieval is a *function*, not a line. That is the whole
//! point: `grep` answers "which lines contain this string", which forces
//! the caller to read an unranked list and rebuild the enclosing
//! definitions themselves. A function-level index answers "which
//! definitions are about this", ranks them, and can say why — which is
//! what fits in an agent's context budget.
//!
//! Three pieces make that work:
//!
//! * [`tokenize`] splits code the way identifiers are actually written.
//!   `parse_diff_range` yields `parse`, `diff`, `range` *and* the joined
//!   form `parsediffrange`; `parseDiffRange` yields the same five tokens.
//!   The joined form is what makes an exact-identifier query sharp — it
//!   occurs in almost no other document, so its IDF is high — while the
//!   sub-tokens keep a partial query working.
//! * [`SearchIndex`] scores with BM25F: one document, several fields
//!   (name, path, signature, doc, body), each with its own weight and
//!   its own length normalisation. A name match and a body match are not
//!   the same evidence, and a single-field BM25 over concatenated text
//!   cannot tell them apart.
//! * [`FuzzyOptions`] expands a query term the vocabulary does not
//!   contain into the nearest terms it does, by Dice overlap of
//!   character trigrams. This is the half-remembered-identifier case
//!   (`parse_diff_rng`), and it is also what makes queries in scripts
//!   without word boundaries — Japanese comments, say — match a longer
//!   surrounding token.
//!
//! Nothing here is persisted. The index is built per run from the
//! corpus the caller already parsed, so it is never stale; the cost it
//! saves — an inverted file on disk that must be invalidated — is the
//! cost that makes stale-index search worse than `grep` rather than
//! better.

use std::collections::{BTreeSet, HashMap};

use crate::naming::identifier_tokens;

/// Fields a [`SearchDocument`] is split into.
///
/// Fields exist so evidence can be weighted by where it appears: a
/// query term in the function's *name* is a much stronger signal than
/// the same term buried in its body, and BM25F is the standard way to
/// say so without the ad-hoc "search names, then fall back to bodies"
/// staging.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SearchField {
    /// The function's own name.
    Name,
    /// The source file's display path.
    Path,
    /// Parameter names, type paths, generics, implemented trait.
    Signature,
    /// Doc comment attached to the definition.
    Doc,
    /// The definition's source text.
    Body,
}

/// Number of fields in [`SearchField`].
pub const FIELD_COUNT: usize = 5;

impl SearchField {
    /// Every field, in slot order.
    pub const ALL: [SearchField; FIELD_COUNT] = [
        Self::Name,
        Self::Path,
        Self::Signature,
        Self::Doc,
        Self::Body,
    ];

    /// Stable lowercase spelling, used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Path => "path",
            Self::Signature => "signature",
            Self::Doc => "doc",
            Self::Body => "body",
        }
    }

    /// Index of this field in the per-field arrays.
    const fn slot(self) -> usize {
        match self {
            Self::Name => 0,
            Self::Path => 1,
            Self::Signature => 2,
            Self::Doc => 3,
            Self::Body => 4,
        }
    }
}

/// One indexable unit: a function projected into the fields BM25F
/// scores over.
///
/// Callers own the text; the index borrows it only while building.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchDocument {
    pub name: String,
    pub path: String,
    pub signature: String,
    pub doc: String,
    pub body: String,
}

impl SearchDocument {
    fn field(&self, field: SearchField) -> &str {
        match field {
            SearchField::Name => &self.name,
            SearchField::Path => &self.path,
            SearchField::Signature => &self.signature,
            SearchField::Doc => &self.doc,
            SearchField::Body => &self.body,
        }
    }
}

/// BM25F scoring parameters.
///
/// `k1` is the usual term-frequency saturation constant. The per-field
/// arrays are indexed by [`SearchField::slot`].
#[derive(Debug, Clone, PartialEq)]
pub struct Bm25Options {
    /// Term-frequency saturation. Higher values let repeated terms keep
    /// contributing; the classic range is 1.2–2.0.
    pub k1: f64,
    /// Per-field evidence weight, relative to the body field's 1.0.
    pub field_weights: [f64; FIELD_COUNT],
    /// Per-field length normalisation, in `[0.0, 1.0]`. `0.0` disables
    /// it, which is the right answer for fields whose length carries no
    /// information (a two-token name is not "more focused" than a
    /// four-token one).
    pub field_b: [f64; FIELD_COUNT],
}

impl Default for Bm25Options {
    fn default() -> Self {
        Self {
            k1: 1.2,
            // Name dominates, path acts as a file-level prior (it does
            // not discriminate *within* a file, and is weighted for
            // that), signature and doc state intent, body is the floor.
            field_weights: [5.0, 1.5, 2.0, 2.0, 1.0],
            field_b: [0.0, 0.0, 0.5, 0.5, 0.75],
        }
    }
}

/// Character-trigram expansion of query terms.
///
/// Two different metrics do two different jobs here, which is what lets
/// one mechanism cover both the misspelling case and the fragment case:
///
/// * **Containment** — what share of the *query's* trigrams the
///   candidate has — decides whether a term qualifies at all. It is
///   asymmetric on purpose: `リトライ` is fully contained in the longer
///   token a script without word boundaries produces, and containment
///   is the only metric that says so.
/// * **Dice** — symmetric overlap — sets how much the expansion is
///   worth. A candidate the same length as the query is strong
///   evidence; a fragment of a much longer term is weak evidence, and
///   Dice scores it that way without a second threshold.
///
/// Expansion is a recall device, never a precision one: an expanded
/// term contributes at most [`Self::weight`] times what a literal match
/// would, so a document that actually spells the query always outranks
/// one that merely spells something similar.
#[derive(Debug, Clone, PartialEq)]
pub struct FuzzyOptions {
    /// Also expand terms the vocabulary *does* contain. Off by default:
    /// expanding a term that already matched adds noise for no recall.
    /// Turning it on buys morphological reach (`retry` → `retries`).
    pub expand_known_terms: bool,
    /// Minimum share of the query term's trigrams the candidate must
    /// carry.
    pub min_containment: f64,
    /// Maximum expansions per query term, best Dice first.
    pub max_expansions: usize,
    /// Score multiplier applied to an expanded term, on top of its Dice
    /// similarity.
    pub weight: f64,
    /// Terms shorter than this are never expanded. Four characters is
    /// two trigrams, the shortest profile where containment means more
    /// than "this substring occurs somewhere".
    pub min_term_chars: usize,
}

impl Default for FuzzyOptions {
    fn default() -> Self {
        Self {
            expand_known_terms: false,
            min_containment: 0.6,
            max_expansions: 3,
            weight: 0.5,
            min_term_chars: 4,
        }
    }
}

/// Everything [`SearchIndex::build`] needs beyond the corpus.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexOptions {
    pub bm25: Bm25Options,
    /// `None` disables n-gram expansion, and skips building the trigram
    /// index entirely.
    pub fuzzy: Option<FuzzyOptions>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            bm25: Bm25Options::default(),
            fuzzy: Some(FuzzyOptions::default()),
        }
    }
}

/// One document's posting for one term: which fields it occurred in,
/// and how often in each.
#[derive(Debug)]
struct Posting {
    document: u32,
    field_tf: [u32; FIELD_COUNT],
}

/// Why one query term contributed to a hit.
///
/// This is the part `grep` structurally cannot emit: a ranked result is
/// only actionable if the reader can see whether it ranked because the
/// function is *named* after the query or because the query appears
/// once in its body.
#[derive(Debug, Clone, PartialEq)]
pub struct TermScore {
    /// The token as the query spelled it.
    pub query_term: String,
    /// The indexed term that matched — equal to `query_term` unless the
    /// match came from n-gram expansion.
    pub matched_term: String,
    /// `1.0` for a literal match, the Dice overlap for an expansion.
    pub similarity: f64,
    /// This term's contribution to the document's score.
    pub score: f64,
    /// Fields the term occurred in, in [`SearchField::ALL`] order.
    pub fields: Vec<SearchField>,
}

impl TermScore {
    /// Whether this contribution came from n-gram expansion rather than
    /// a literal match.
    pub fn is_expansion(&self) -> bool {
        self.query_term != self.matched_term
    }
}

/// One scored document.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    /// Index into the slice handed to [`SearchIndex::build`].
    pub document: usize,
    pub score: f64,
    /// Per-term breakdown, strongest first.
    pub terms: Vec<TermScore>,
}

/// A query term resolved against the vocabulary.
#[derive(Debug)]
struct PlannedTerm {
    query_term: String,
    term_id: usize,
    similarity: f64,
    weight: f64,
}

/// An in-memory BM25F index over a function corpus.
///
/// Built per run and thrown away: see the module docs for why there is
/// no persistence.
#[derive(Debug)]
pub struct SearchIndex {
    options: IndexOptions,
    document_count: usize,
    /// Vocabulary, indexed by term id.
    terms: Vec<String>,
    term_ids: HashMap<String, usize>,
    /// Distinct trigram count per term, parallel to `terms`.
    trigram_counts: Vec<u32>,
    /// Postings by term id, each ascending by document.
    postings: Vec<Vec<Posting>>,
    /// Per-document token counts per field.
    field_lengths: Vec<[u32; FIELD_COUNT]>,
    avg_field_length: [f64; FIELD_COUNT],
    /// Trigram → term ids, ascending. Empty when fuzzy is disabled.
    trigrams: HashMap<String, Vec<u32>>,
}

impl SearchIndex {
    /// Index `documents`, which the caller keeps: hits refer back to
    /// them by position.
    pub fn build(documents: &[SearchDocument], options: IndexOptions) -> Self {
        let mut terms: Vec<String> = Vec::new();
        let mut term_ids: HashMap<String, usize> = HashMap::new();
        let mut postings: Vec<Vec<Posting>> = Vec::new();
        let mut field_lengths: Vec<[u32; FIELD_COUNT]> = Vec::with_capacity(documents.len());
        let mut total_field_length = [0u64; FIELD_COUNT];

        for (document_index, document) in documents.iter().enumerate() {
            let mut lengths = [0u32; FIELD_COUNT];
            let mut document_terms: HashMap<usize, [u32; FIELD_COUNT]> = HashMap::new();
            for field in SearchField::ALL {
                let slot = field.slot();
                for token in tokenize(document.field(field)) {
                    lengths[slot] = lengths[slot].saturating_add(1);
                    let term_id = *term_ids.entry(token.clone()).or_insert_with(|| {
                        terms.push(token);
                        postings.push(Vec::new());
                        terms.len() - 1
                    });
                    let field_tf = document_terms.entry(term_id).or_insert([0; FIELD_COUNT]);
                    field_tf[slot] = field_tf[slot].saturating_add(1);
                }
            }
            // Documents are visited in order and each (term, document)
            // pair is appended once, so every posting list comes out
            // ascending by document without a sort.
            for (term_id, field_tf) in document_terms {
                postings[term_id].push(Posting {
                    document: u32::try_from(document_index).unwrap_or(u32::MAX),
                    field_tf,
                });
            }
            for (total, len) in total_field_length.iter_mut().zip(lengths) {
                *total += u64::from(len);
            }
            field_lengths.push(lengths);
        }

        let document_count = documents.len();
        let mut avg_field_length = [0.0; FIELD_COUNT];
        for (avg, total) in avg_field_length.iter_mut().zip(total_field_length) {
            *avg = if document_count == 0 {
                0.0
            } else {
                total as f64 / document_count as f64
            };
        }

        let trigram_counts = terms.iter().map(|t| trigram_set(t).len() as u32).collect();
        let trigrams = if options.fuzzy.is_some() {
            build_trigram_index(&terms)
        } else {
            HashMap::new()
        };

        Self {
            options,
            document_count,
            terms,
            term_ids,
            trigram_counts,
            postings,
            field_lengths,
            avg_field_length,
            trigrams,
        }
    }

    /// Number of indexed documents.
    pub fn document_count(&self) -> usize {
        self.document_count
    }

    /// Size of the vocabulary.
    pub fn term_count(&self) -> usize {
        self.terms.len()
    }

    /// Score `query` and return at most `limit` hits, strongest first.
    ///
    /// Ties break on document order, so the same corpus and query always
    /// produce the same list.
    ///
    /// # Examples
    ///
    /// ```
    /// use lens_domain::search::{IndexOptions, SearchDocument, SearchIndex};
    ///
    /// let docs = vec![
    ///     SearchDocument { name: "parse_diff_range".into(), ..Default::default() },
    ///     SearchDocument { name: "render_report".into(), ..Default::default() },
    /// ];
    /// let index = SearchIndex::build(&docs, IndexOptions::default());
    ///
    /// // An exact identifier ranks its own definition first ...
    /// let hits = index.search("parse_diff_range", 10);
    /// assert_eq!(hits[0].document, 0);
    ///
    /// // ... and so does a misspelling of it, through n-gram expansion.
    /// let hits = index.search("parse_diff_rng", 10);
    /// assert_eq!(hits[0].document, 0);
    /// assert!(hits[0].terms.iter().any(|t| t.is_expansion()));
    /// ```
    pub fn search(&self, query: &str, limit: usize) -> Vec<SearchHit> {
        let mut scores: HashMap<u32, f64> = HashMap::new();
        let mut matches: HashMap<u32, Vec<TermScore>> = HashMap::new();

        for planned in self.plan(query) {
            let idf = self.idf(planned.term_id);
            if idf <= 0.0 {
                continue;
            }
            for posting in &self.postings[planned.term_id] {
                let tf = self.weighted_tf(posting);
                if tf <= 0.0 {
                    continue;
                }
                let score = planned.weight * idf * tf / (self.options.bm25.k1 + tf);
                *scores.entry(posting.document).or_insert(0.0) += score;
                matches
                    .entry(posting.document)
                    .or_default()
                    .push(TermScore {
                        query_term: planned.query_term.clone(),
                        matched_term: self.terms[planned.term_id].clone(),
                        similarity: planned.similarity,
                        score,
                        fields: SearchField::ALL
                            .into_iter()
                            .filter(|field| posting.field_tf[field.slot()] > 0)
                            .collect(),
                    });
            }
        }

        let mut hits: Vec<SearchHit> = scores
            .into_iter()
            .map(|(document, score)| {
                let mut terms = matches.remove(&document).unwrap_or_default();
                terms.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then_with(|| a.matched_term.cmp(&b.matched_term))
                });
                SearchHit {
                    document: document as usize,
                    score,
                    terms,
                }
            })
            .collect();
        hits.sort_by(|a, b| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| a.document.cmp(&b.document))
        });
        hits.truncate(limit);
        hits
    }

    /// Resolve the query's tokens against the vocabulary, adding n-gram
    /// expansions where configured. Duplicate tokens are dropped: the
    /// tokenizer already emits a joined form alongside its sub-tokens,
    /// which is the only term repetition a code query should be
    /// weighted for.
    fn plan(&self, query: &str) -> Vec<PlannedTerm> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut planned = Vec::new();
        for token in tokenize(query) {
            if !seen.insert(token.clone()) {
                continue;
            }
            let known = self.term_ids.get(&token).copied();
            if let Some(term_id) = known {
                planned.push(PlannedTerm {
                    query_term: token.clone(),
                    term_id,
                    similarity: 1.0,
                    weight: 1.0,
                });
            }
            let Some(fuzzy) = &self.options.fuzzy else {
                continue;
            };
            if known.is_some() && !fuzzy.expand_known_terms {
                continue;
            }
            for (term_id, similarity) in self.expansions(&token, fuzzy) {
                planned.push(PlannedTerm {
                    query_term: token.clone(),
                    term_id,
                    similarity,
                    weight: fuzzy.weight * similarity,
                });
            }
        }
        planned
    }

    /// Vocabulary terms carrying at least `fuzzy.min_containment` of
    /// `term`'s trigrams, best Dice first, capped. The returned score is
    /// the Dice similarity, which is what the expansion's weight is
    /// scaled by.
    fn expansions(&self, term: &str, fuzzy: &FuzzyOptions) -> Vec<(usize, f64)> {
        if term.chars().count() < fuzzy.min_term_chars {
            return Vec::new();
        }
        let query_grams = trigram_set(term);
        if query_grams.is_empty() {
            return Vec::new();
        }
        let mut shared: HashMap<usize, usize> = HashMap::new();
        for gram in &query_grams {
            for &candidate in self.trigrams.get(gram).map_or(&[][..], Vec::as_slice) {
                *shared.entry(candidate as usize).or_insert(0) += 1;
            }
        }
        let mut scored: Vec<(usize, f64)> = shared
            .into_iter()
            .filter(|&(term_id, _)| self.terms[term_id] != term)
            .filter_map(|(term_id, common)| {
                let containment = common as f64 / query_grams.len() as f64;
                if containment < fuzzy.min_containment {
                    return None;
                }
                let candidate_grams = self.trigram_counts[term_id] as usize;
                Some((
                    term_id,
                    dice_similarity(common, query_grams.len(), candidate_grams),
                ))
            })
            .collect();
        scored.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| self.terms[a.0].cmp(&self.terms[b.0]))
        });
        scored.truncate(fuzzy.max_expansions);
        scored
    }

    /// Lucene-style IDF: always positive, so a term present in every
    /// document contributes ~0 rather than a negative score. This is
    /// also why the index carries no stopword list — language keywords
    /// are in nearly every document and fall out on their own.
    fn idf(&self, term_id: usize) -> f64 {
        let document_frequency = self.postings[term_id].len() as f64;
        let total = self.document_count as f64;
        (1.0 + (total - document_frequency + 0.5) / (document_frequency + 0.5)).ln()
    }

    /// The BM25F pseudo-frequency: each field's raw count scaled by its
    /// weight and divided by that field's own length normalisation,
    /// summed. Saturation is applied once to the sum, which is what
    /// separates BM25F from summing per-field BM25 scores.
    fn weighted_tf(&self, posting: &Posting) -> f64 {
        let lengths = &self.field_lengths[posting.document as usize];
        let mut acc = 0.0;
        for (slot, &tf) in posting.field_tf.iter().enumerate() {
            if tf == 0 {
                continue;
            }
            // No zero-average guard: `tf > 0` means this document had a
            // token in the field, so the field's corpus total — and
            // therefore its average — cannot be zero. A guard here would
            // be a branch no input reaches.
            let avg = self.avg_field_length[slot];
            let b = self.options.bm25.field_b[slot];
            let norm = 1.0 - b + b * f64::from(lengths[slot]) / avg;
            // `b` is a public knob, so a caller can put it outside
            // `[0, 1]` and drive the normalisation to zero or below.
            // That one is reachable.
            if norm <= 0.0 {
                continue;
            }
            acc += self.options.bm25.field_weights[slot] * f64::from(tf) / norm;
        }
        acc
    }
}

/// Split source text into search terms.
///
/// Words break on anything that is not alphanumeric or `_`; each word
/// then splits on `_` and camelCase transitions via
/// [`identifier_tokens`]. A word that produced more than one sub-token
/// also contributes the *joined* form, so `parse_diff_range`,
/// `parseDiffRange` and `ParseDiffRange` all index and query as the
/// same `parsediffrange` — one rare, high-IDF term that makes an exact
/// identifier query sharp.
///
/// # Examples
///
/// ```
/// use lens_domain::search::tokenize;
///
/// assert_eq!(
///     tokenize("fn parse_diff_range("),
///     ["fn", "parsediffrange", "parse", "diff", "range"],
/// );
/// // Spelling convention does not change the terms.
/// assert_eq!(tokenize("parseDiffRange"), tokenize("parse_diff_range"));
/// ```
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for word in text.split(|ch: char| !(ch.is_alphanumeric() || ch == '_')) {
        if word.is_empty() {
            continue;
        }
        let parts = identifier_tokens(word);
        if parts.len() > 1 {
            out.push(parts.concat());
        }
        out.extend(parts);
    }
    out
}

/// Dice coefficient of two trigram sets from their intersection size
/// and cardinalities, in `[0.0, 1.0]`.
///
/// Symmetric, and penalised by a length gap: a four-character fragment
/// of a twenty-character token scores low even though the fragment is
/// wholly contained. That penalty is deliberate — it is what keeps a
/// fragment match worth less than a near-identical spelling.
fn dice_similarity(common: usize, a_len: usize, b_len: usize) -> f64 {
    let total = a_len + b_len;
    if total == 0 {
        return 0.0;
    }
    2.0 * common as f64 / total as f64
}

/// Distinct character trigrams of `term`. Character-based rather than
/// byte-based so a multi-byte script is not sliced mid-codepoint.
fn trigram_set(term: &str) -> BTreeSet<String> {
    let chars: Vec<char> = term.chars().collect();
    chars
        .windows(3)
        .map(|window| window.iter().collect::<String>())
        .collect()
}

/// Invert the vocabulary by trigram. Term ids land ascending in each
/// bucket because terms are visited in id order.
fn build_trigram_index(terms: &[String]) -> HashMap<String, Vec<u32>> {
    let mut index: HashMap<String, Vec<u32>> = HashMap::new();
    for (term_id, term) in terms.iter().enumerate() {
        let Ok(term_id) = u32::try_from(term_id) else {
            break;
        };
        for gram in trigram_set(term) {
            index.entry(gram).or_default().push(term_id);
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn doc(name: &str, body: &str) -> SearchDocument {
        SearchDocument {
            name: name.to_owned(),
            body: body.to_owned(),
            ..Default::default()
        }
    }

    fn index(documents: &[SearchDocument]) -> SearchIndex {
        SearchIndex::build(documents, IndexOptions::default())
    }

    fn assert_close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-12,
            "expected {expected}, got {actual}",
        );
    }

    /// Pin the whole BM25F chain on a corpus small enough to compute by
    /// hand. Ordering assertions elsewhere survive most arithmetic
    /// perturbations of these formulas; this is the test that does not.
    ///
    /// Two documents, `name` the only populated field. For the query
    /// `alpha`, present in one of them:
    ///
    ///   idf = ln(1 + (2 - 1 + 0.5) / (1 + 0.5))          = ln 2
    ///   ~tf = 5.0 × 1 / (1 - 0 + 0 × len/avg)            = 5.0
    ///   score = 1.0 × idf × ~tf / (k1 + ~tf)             = ln 2 × 5/6.2
    #[test]
    fn a_name_only_score_matches_the_bm25f_formula() {
        let documents = vec![doc("alpha", ""), doc("beta", "")];
        let hits = index(&documents).search("alpha", 10);
        assert_eq!(hits.len(), 1);
        let expected = 2.0f64.ln() * 5.0 / (1.2 + 5.0);
        assert_close(hits[0].score, expected);
        assert_close(hits[0].terms[0].score, expected);
    }

    /// The name field sets `b = 0`, so the case above cannot see the
    /// length-normalisation term at all. The body field sets `b = 0.75`,
    /// which makes the same term worth less in a longer body:
    ///
    ///   avg body length = (1 + 3) / 2                    = 2
    ///   idf = ln(1 + (2 - 2 + 0.5) / (2 + 0.5))          = ln 1.2
    ///   short: ~tf = 1 / (1 - 0.75 + 0.75 × 1/2)         = 1.6
    ///   long:  ~tf = 1 / (1 - 0.75 + 0.75 × 3/2)         = 8/11
    #[test]
    fn a_longer_field_dilutes_the_same_term() {
        let documents = vec![doc("", "alpha"), doc("", "alpha beta gamma")];
        let hits = index(&documents).search("alpha", 10);
        let idf = 1.2f64.ln();
        assert_eq!(hits[0].document, 0, "hits: {hits:?}");
        assert_close(hits[0].score, idf * 1.6 / (1.2 + 1.6));
        let long = 1.0 / (1.0 - 0.75 + 0.75 * 3.0 / 2.0);
        assert_close(hits[1].score, idf * long / (1.2 + long));
    }

    /// Every other formula case uses a term appearing once, where the
    /// field weight multiplying the count is indistinguishable from it
    /// dividing one. Two occurrences separate them — and outrank one:
    ///
    ///   avg body length = (2 + 2) / 2                    = 2
    ///   idf = ln(1 + (2 - 2 + 0.5) / (2 + 0.5))          = ln 1.2
    ///   norm = 1 - 0.75 + 0.75 × 2/2                     = 1
    ///   twice: ~tf = 1.0 × 2 / 1                         = 2
    ///   once:  ~tf = 1.0 × 1 / 1                         = 1
    #[test]
    fn a_repeated_term_in_one_field_outranks_a_single_occurrence() {
        let documents = vec![doc("", "alpha alpha"), doc("", "alpha beta")];
        let hits = index(&documents).search("alpha", 10);
        let idf = 1.2f64.ln();
        assert_eq!(hits[0].document, 0, "hits: {hits:?}");
        assert_close(hits[0].score, idf * 2.0 / (1.2 + 2.0));
        assert_close(hits[1].score, idf * 1.0 / (1.2 + 1.0));
    }

    /// A field nothing in the corpus populates has a zero average, and
    /// must simply contribute nothing — not a division by zero that
    /// poisons the whole score.
    #[test]
    fn a_field_no_document_populates_never_reaches_the_normalisation() {
        let documents = vec![doc("alpha", ""), doc("beta", "")];
        let hits = index(&documents).search("alpha", 10);
        assert!(hits[0].score.is_finite(), "hits: {hits:?}");
        assert_eq!(
            hits[0].terms[0].fields,
            [SearchField::Name],
            "the empty fields must not appear as evidence",
        );
    }

    /// Containment is a threshold, so it has to be pinned at the
    /// threshold. Against the four trigrams of `abcdef`, the candidates
    /// carry 4/4, 3/4 and 2/4 of them — so a 0.75 cut must admit
    /// exactly the first two.
    #[test]
    fn containment_admits_a_candidate_exactly_at_the_cut() {
        let documents = vec![doc("abcdefg", ""), doc("abcdezz", ""), doc("abcdxy", "")];
        let index = SearchIndex::build(
            &documents,
            IndexOptions {
                fuzzy: Some(FuzzyOptions {
                    min_containment: 0.75,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let hits = index.search("abcdef", 10);
        assert_eq!(
            hits.iter().map(|h| h.document).collect::<Vec<_>>(),
            [0, 1],
            "2/4 containment is below the cut, 3/4 is exactly at it: {hits:?}",
        );
    }

    #[rstest]
    #[case("load_user_id", vec!["loaduserid", "load", "user", "id"])]
    #[case("loadUserId", vec!["loaduserid", "load", "user", "id"])]
    #[case("LoadUserID", vec!["loaduserid", "load", "user", "id"])]
    // A single-part word contributes no joined duplicate.
    #[case("search", vec!["search"])]
    // Punctuation and whitespace are boundaries; `_` is not.
    #[case("a.b(c_d)", vec!["a", "b", "cd", "c", "d"])]
    // Digits stay attached to the run they were written in.
    #[case("utf8_len", vec!["utf8len", "utf8", "len"])]
    #[case("", Vec::<&str>::new())]
    #[case("!!! ???", Vec::<&str>::new())]
    fn tokenize_splits_identifiers_and_adds_the_joined_form(
        #[case] text: &str,
        #[case] expected: Vec<&str>,
    ) {
        assert_eq!(tokenize(text), expected);
    }

    /// The joined form is the whole reason an exact-identifier query is
    /// sharp: it must survive the round trip through both the document
    /// and the query side of the tokenizer.
    #[test]
    fn an_exact_identifier_outranks_a_document_sharing_only_sub_tokens() {
        let documents = vec![
            doc("parse_diff_range", "let range = 1;"),
            doc("parse_range", "diff the range"),
            doc("diff_range", "parse it"),
        ];
        let hits = index(&documents).search("parse_diff_range", 10);
        assert_eq!(hits[0].document, 0, "hits: {hits:?}");
        assert!(hits[0].score > hits[1].score * 1.5, "hits: {hits:?}");
    }

    /// Field weights are the point of BM25F: the same term in a name
    /// must outrank it in a body.
    #[test]
    fn a_name_match_outranks_a_body_match() {
        let documents = vec![
            doc("unrelated", "retry retry retry retry retry"),
            doc("retry", "unrelated"),
        ];
        let hits = index(&documents).search("retry", 10);
        assert_eq!(hits[0].document, 1, "hits: {hits:?}");
    }

    #[test]
    fn hits_report_which_fields_matched() {
        let documents = vec![SearchDocument {
            name: "resolve_symbol".to_owned(),
            doc: "Resolve a symbol.".to_owned(),
            body: "fn resolve_symbol() {}".to_owned(),
            ..Default::default()
        }];
        let index = index(&documents);
        assert_eq!(index.document_count(), 1);
        // `resolvesymbol`, `resolve`, `symbol`, `a`, `fn` — the joined
        // form plus its parts, counted once across every field.
        assert_eq!(index.term_count(), 5);

        let hits = index.search("resolve", 10);
        let term = hits[0]
            .terms
            .iter()
            .find(|t| t.matched_term == "resolve")
            .expect("the literal term must be reported");
        assert_eq!(
            term.fields,
            [SearchField::Name, SearchField::Doc, SearchField::Body],
        );
        assert_eq!(term.similarity, 1.0);
        assert!(!term.is_expansion());
    }

    /// A term the corpus never spells is the case n-gram expansion
    /// exists for. Without it the query returns nothing at all.
    #[test]
    fn a_misspelled_identifier_still_finds_its_definition() {
        let documents = vec![doc("resolver", "lexical lookup"), doc("unrelated", "")];
        let hits = index(&documents).search("resolvr", 10);
        assert_eq!(hits[0].document, 0, "hits: {hits:?}");
        assert!(hits[0].terms.iter().any(TermScore::is_expansion));

        let strict = SearchIndex::build(
            &documents,
            IndexOptions {
                fuzzy: None,
                ..Default::default()
            },
        );
        assert!(strict.search("resolvr", 10).is_empty());
    }

    /// Scripts without word boundaries tokenize to one long term, so a
    /// substring query only reaches them through the n-gram index.
    #[test]
    fn a_substring_query_reaches_a_token_with_no_word_boundaries() {
        let documents = vec![doc("retry", "リトライ処理をここで行う"), doc("other", "")];
        let hits = index(&documents).search("リトライ", 10);
        assert_eq!(hits[0].document, 0, "hits: {hits:?}");
    }

    /// Expansion is recall, not precision: whatever it adds must never
    /// beat the document that actually spells the query.
    #[test]
    fn an_expanded_match_never_outranks_a_literal_one() {
        let documents = vec![doc("resolver", ""), doc("resolve", "")];
        let always = SearchIndex::build(
            &documents,
            IndexOptions {
                fuzzy: Some(FuzzyOptions {
                    expand_known_terms: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let hits = always.search("resolve", 10);
        assert_eq!(hits[0].document, 1, "hits: {hits:?}");
        assert_eq!(hits.len(), 2, "the expansion must still be reachable");
    }

    #[test]
    fn known_terms_are_not_expanded_by_default() {
        let documents = vec![doc("resolver", ""), doc("resolve", "")];
        let hits = index(&documents).search("resolve", 10);
        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert_eq!(hits[0].document, 1);
    }

    #[test]
    fn results_are_capped_and_ordered_deterministically() {
        let documents: Vec<SearchDocument> = (0..10).map(|_| doc("handler", "handler")).collect();
        let hits = index(&documents).search("handler", 3);
        assert_eq!(
            hits.iter().map(|h| h.document).collect::<Vec<_>>(),
            [0, 1, 2],
            "identical documents must tie-break on position",
        );
    }

    #[test]
    fn a_query_with_no_indexed_term_scores_nothing() {
        let documents = vec![doc("handler", "handler")];
        assert!(index(&documents).search("zzzz", 10).is_empty());
        assert!(index(&documents).search("", 10).is_empty());
    }

    #[test]
    fn an_empty_corpus_answers_without_dividing_by_zero() {
        let index = index(&[]);
        assert_eq!(index.document_count(), 0);
        assert_eq!(index.term_count(), 0);
        assert!(index.search("anything", 10).is_empty());
    }

    /// A term in every document carries no information, and the IDF
    /// form must express that as ~0 rather than as a negative score
    /// that would push matching documents *down*.
    #[test]
    fn a_term_in_every_document_contributes_almost_nothing() {
        let documents: Vec<SearchDocument> =
            (0..20).map(|i| doc(&format!("f{i}"), "common")).collect();
        let hits = index(&documents).search("common", 5);
        for hit in &hits {
            assert!(hit.score < 0.1, "hits: {hits:?}");
        }
    }

    /// Containment qualifies an expansion, Dice prices it. A fragment
    /// wholly contained in a long token must therefore be *reachable*
    /// but *cheap* — the property that lets one mechanism serve both
    /// misspellings and substrings without a second threshold.
    #[test]
    fn a_fragment_of_a_long_token_scores_below_a_near_identical_spelling() {
        let documents = vec![
            doc("a", "リトライ処理をここで行う"),
            doc("b", "リトライ"),
            doc("c", ""),
        ];
        let always = SearchIndex::build(
            &documents,
            IndexOptions {
                fuzzy: Some(FuzzyOptions {
                    expand_known_terms: true,
                    ..Default::default()
                }),
                ..Default::default()
            },
        );
        let hits = always.search("リトライ", 10);
        assert_eq!(
            hits.iter().map(|h| h.document).collect::<Vec<_>>(),
            [1, 0],
            "hits: {hits:?}",
        );
    }

    #[rstest]
    #[case(3, 3, 3, 1.0)]
    #[case(0, 3, 3, 0.0)]
    #[case(2, 2, 10, 1.0 / 3.0)]
    #[case(0, 0, 0, 0.0)]
    fn dice_similarity_is_symmetric_overlap_penalised_by_length_gap(
        #[case] common: usize,
        #[case] a_len: usize,
        #[case] b_len: usize,
        #[case] expected: f64,
    ) {
        assert_eq!(dice_similarity(common, a_len, b_len), expected);
        assert_eq!(
            dice_similarity(common, a_len, b_len),
            dice_similarity(common, b_len, a_len),
        );
    }

    #[test]
    fn field_slots_are_distinct_and_cover_every_field() {
        let slots: BTreeSet<usize> = SearchField::ALL
            .into_iter()
            .map(SearchField::slot)
            .collect();
        assert_eq!(slots, (0..FIELD_COUNT).collect::<BTreeSet<_>>());
        let names: BTreeSet<&str> = SearchField::ALL
            .into_iter()
            .map(SearchField::as_str)
            .collect();
        assert_eq!(names.len(), FIELD_COUNT);
    }
}

//! Name-anchored pairing for `analyze similarity --paired-by`.
//!
//! Threshold clustering answers "what is still similar?". It has an
//! inherent blind spot: two implementations of the same thing that have
//! drifted apart score *lower*, so the more urgent a missed sync is, the
//! less likely it is to be reported. This module inverts the question to
//! "what should be similar but no longer is?" — functions are matched by a
//! name key first and scored second, so every matched pair is reported
//! regardless of threshold and the most-drifted ones sort first.

use std::collections::HashMap;

use lens_domain::identifier_tokens;

use super::OwnedUnit;

/// How two functions are decided to be siblings — i.e. parallel
/// implementations that ought to stay in sync.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PairKey {
    /// Owner-qualified name, normalized: `Summary::from` and
    /// `JsSummary::from` share the key `summary::from` because case and
    /// separator conventions are folded away and binding affixes
    /// (`Js`, `Py`, `Wasm`, `Napi`, `Node`, `Ts`) are stripped from the
    /// owner. The tight key: few, mostly meaningful matches.
    #[default]
    #[value(alias = "name")]
    #[serde(alias = "name")]
    Qualified,
    /// The method segment alone, normalized: every `::from` in the tree
    /// keys as `from`. The loose key — it finds siblings whose owning
    /// types were renamed past recognition, at the cost of grouping
    /// unrelated functions that merely share a common method name.
    Method,
}

impl PairKey {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Qualified => "qualified",
            Self::Method => "method",
        }
    }
}

/// Language-binding affixes stripped from an owner segment before it
/// becomes part of a [`PairKey::Qualified`] key. These are the prefixes
/// (and occasional suffixes) that multi-target codebases hang on
/// otherwise-identical mirror types: `Article` / `JsArticle` /
/// `PyArticle`. Kept deliberately short — every entry here is a chance to
/// collide two genuinely different types.
const BINDING_AFFIXES: &[&str] = &["js", "ts", "py", "python", "wasm", "napi", "node"];

/// Sibling key for `function`, or `None` when its name has no usable
/// tokens (an anonymous or synthetic entry).
pub(super) fn pair_key(function: &OwnedUnit, key: PairKey) -> Option<String> {
    let name = function.name();
    // A type unit's name is the type itself, so it gets the owner
    // treatment: binding affixes stripped, making `Summary` /
    // `JsSummary` / `PySummary` siblings. `PairKey::Method` is rejected
    // for the types target before pairing starts.
    if function.is_type() {
        return normalize_owner(name);
    }
    let (owner, method) = match name.rsplit_once("::") {
        Some((owner, method)) => (Some(owner), method),
        None => (None, name),
    };
    let method = normalize_segment(method)?;
    match (key, owner) {
        (PairKey::Method, _) | (PairKey::Qualified, None) => Some(method),
        (PairKey::Qualified, Some(owner)) => Some(match normalize_owner(owner) {
            Some(owner) => format!("{owner}::{method}"),
            None => method,
        }),
    }
}

/// Fold an identifier to its lowercase `_`-joined tokens so `getUser`,
/// `get_user`, and `GetUser` all land on `get_user`. `None` when the
/// identifier carries no alphanumeric tokens at all.
fn normalize_segment(segment: &str) -> Option<String> {
    let tokens = identifier_tokens(segment);
    (!tokens.is_empty()).then(|| tokens.join("_"))
}

/// [`normalize_segment`] plus binding-affix stripping, for the owning
/// type of a method. An owner made *entirely* of affixes (`Js`, `Wasm`)
/// keeps its tokens rather than collapsing to nothing — a type literally
/// named `Js` is a real distinction, not a prefix.
fn normalize_owner(owner: &str) -> Option<String> {
    let tokens = identifier_tokens(owner);
    let stripped: Vec<&String> = tokens
        .iter()
        .filter(|token| !BINDING_AFFIXES.contains(&token.as_str()))
        .collect();
    let kept: Vec<&str> = if stripped.is_empty() {
        tokens.iter().map(String::as_str).collect()
    } else {
        stripped.into_iter().map(String::as_str).collect()
    };
    (!kept.is_empty()).then(|| kept.join("_"))
}

/// One name-matched pair, referencing its key by index into
/// [`PairedCandidates::keys`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PairedCandidate {
    pub i: usize,
    pub j: usize,
    pub key: usize,
}

#[derive(Debug, Default)]
pub(super) struct PairedCandidates {
    /// Distinct keys that produced at least one pair, sorted.
    pub keys: Vec<String>,
    pub pairs: Vec<PairedCandidate>,
    pub eligible_function_count: usize,
    /// Same-name functions that were skipped because both sides live in
    /// the same file. Reported so a run that finds nothing can say
    /// whether there was nothing to find or only in-file namesakes.
    pub same_file_pair_count: usize,
}

/// Group `corpus` by [`pair_key`] and emit every cross-file pair within a
/// group.
///
/// Same-file pairs are excluded: sibling implementations are a
/// cross-file/cross-crate pattern, and a file's own overloads and
/// same-named helpers would otherwise dominate the loose
/// [`PairKey::Method`] key. Functions shorter than `min_lines` are
/// dropped first, matching the clustering path.
///
/// Output order is deterministic: keys ascending, then corpus index.
pub(super) fn name_matched_pairs(
    corpus: &[OwnedUnit],
    min_lines: usize,
    key: PairKey,
) -> PairedCandidates {
    let mut by_key: HashMap<String, Vec<usize>> = HashMap::new();
    let mut eligible_function_count = 0;
    for (i, function) in corpus.iter().enumerate() {
        if function.line_count() < min_lines {
            continue;
        }
        eligible_function_count += 1;
        if let Some(key) = pair_key(function, key) {
            by_key.entry(key).or_default().push(i);
        }
    }

    // Dropping singletons here is a pure optimization — the pair loop
    // below emits nothing for a one-member group and never records its
    // key — but it keeps the sort off every distinct name in the corpus,
    // which on a large tree is most of them.
    let mut grouped: Vec<(String, Vec<usize>)> = by_key
        .into_iter()
        .filter(|(_, members)| members.len() > 1)
        .collect();
    grouped.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = PairedCandidates {
        eligible_function_count,
        ..PairedCandidates::default()
    };
    for (name, members) in grouped {
        let before = out.pairs.len();
        for (pos, &i) in members.iter().enumerate() {
            for &j in &members[pos + 1..] {
                let (Some(a), Some(b)) = (corpus.get(i), corpus.get(j)) else {
                    continue;
                };
                if a.rel_path == b.rel_path {
                    out.same_file_pair_count += 1;
                    continue;
                }
                out.pairs.push(PairedCandidate {
                    i,
                    j,
                    key: out.keys.len(),
                });
            }
        }
        if out.pairs.len() > before {
            out.keys.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;
    use std::path::PathBuf;

    fn function(name: &str, rel_path: &str, line_count: usize) -> OwnedUnit {
        OwnedUnit {
            file: PathBuf::from(rel_path),
            rel_path: rel_path.to_owned(),
            is_test: false,
            kind: None,
            shape: lens_domain::FunctionShape::from(lens_domain::FunctionDef {
                name: name.to_owned(),
                start_line: 1,
                end_line: line_count,
                is_test: false,
                signature: None,
                doc: None,
                tree: lens_domain::TreeNode::leaf("Block"),
            }),
        }
    }

    #[rstest]
    // Case and separator conventions fold away, so the same function
    // matches across languages.
    #[case::snake_case("get_user", PairKey::Qualified, "get_user")]
    #[case::camel_case("getUser", PairKey::Qualified, "get_user")]
    #[case::pascal_case("GetUser", PairKey::Qualified, "get_user")]
    // The motivating case: a NAPI type and its WASM mirror.
    #[case::plain_owner("Summary::from", PairKey::Qualified, "summary::from")]
    #[case::js_prefixed_owner("JsSummary::from", PairKey::Qualified, "summary::from")]
    #[case::py_prefixed_owner("PySummary::from", PairKey::Qualified, "summary::from")]
    #[case::napi_suffixed_owner("SummaryNapi::from", PairKey::Qualified, "summary::from")]
    // An owner that is nothing but an affix is a real type name, not a
    // prefix, so it keeps its tokens instead of collapsing to `from`.
    #[case::owner_is_all_affix("Js::from", PairKey::Qualified, "js::from")]
    // Affixes are owner-only: a method actually called `js_config`
    // must not become `config`.
    #[case::affix_in_method("Client::jsConfig", PairKey::Qualified, "client::js_config")]
    // The loose key drops the owner entirely.
    #[case::method_key("JsSummary::from", PairKey::Method, "from")]
    #[case::method_key_unqualified("get_user", PairKey::Method, "get_user")]
    fn pair_key_normalizes_names(#[case] name: &str, #[case] key: PairKey, #[case] expected: &str) {
        assert_eq!(
            pair_key(&function(name, "lib.rs", 10), key).as_deref(),
            Some(expected)
        );
    }

    fn type_unit(name: &str) -> OwnedUnit {
        OwnedUnit {
            kind: Some("struct"),
            ..function(name, "lib.rs", 10)
        }
    }

    /// Type units key on the affix-stripped type name itself, so mirror
    /// structs named per binding share a key.
    #[rstest]
    #[case::plain("Summary", "summary")]
    #[case::js_prefixed("JsSummary", "summary")]
    #[case::py_prefixed("PySummary", "summary")]
    #[case::napi_suffixed("SummaryNapi", "summary")]
    #[case::all_affix_keeps_tokens("Js", "js")]
    fn pair_key_strips_binding_affixes_from_type_names(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(
            pair_key(&type_unit(name), PairKey::Qualified).as_deref(),
            Some(expected)
        );
    }

    #[test]
    fn pair_key_rejects_names_without_tokens() {
        assert_eq!(
            pair_key(&function("::", "lib.rs", 10), PairKey::Qualified),
            None
        );
        assert_eq!(pair_key(&function("", "lib.rs", 10), PairKey::Method), None);
    }

    #[test]
    fn name_matched_pairs_groups_cross_file_siblings() {
        let corpus = vec![
            function("Summary::from", "napi.rs", 10),
            function("JsSummary::from", "wasm.rs", 10),
            function("PySummary::from", "py.rs", 10),
            function("Article::parse", "napi.rs", 10),
        ];

        let candidates = name_matched_pairs(&corpus, 5, PairKey::Qualified);

        assert_eq!(candidates.keys, vec!["summary::from".to_owned()]);
        assert_eq!(
            candidates.pairs,
            vec![
                PairedCandidate { i: 0, j: 1, key: 0 },
                PairedCandidate { i: 0, j: 2, key: 0 },
                PairedCandidate { i: 1, j: 2, key: 0 },
            ],
        );
        assert_eq!(candidates.eligible_function_count, 4);
        assert_eq!(candidates.same_file_pair_count, 0);
    }

    #[test]
    fn name_matched_pairs_skips_same_file_namesakes() {
        let corpus = vec![
            function("Summary::from", "lib.rs", 10),
            function("Article::from", "lib.rs", 10),
        ];

        let candidates = name_matched_pairs(&corpus, 5, PairKey::Method);

        assert!(candidates.pairs.is_empty());
        // The key produced a group but no reportable pair, so it must not
        // be carried into the report either.
        assert!(candidates.keys.is_empty());
        assert_eq!(candidates.same_file_pair_count, 1);
    }

    #[test]
    fn name_matched_pairs_applies_min_lines_before_grouping() {
        let corpus = vec![
            function("Summary::from", "napi.rs", 3),
            function("JsSummary::from", "wasm.rs", 10),
        ];

        let candidates = name_matched_pairs(&corpus, 5, PairKey::Qualified);

        assert!(candidates.pairs.is_empty());
        assert_eq!(candidates.eligible_function_count, 1);
    }

    /// Two keys, each with pairs: key indices must address the right
    /// entry of the sorted `keys` list, not just be sequential.
    #[test]
    fn name_matched_pairs_indexes_keys_in_sorted_order() {
        let corpus = vec![
            function("Zeta::from", "a.rs", 10),
            function("Zeta::from", "b.rs", 10),
            function("Alpha::parse", "a.rs", 10),
            function("Alpha::parse", "b.rs", 10),
        ];

        let candidates = name_matched_pairs(&corpus, 5, PairKey::Qualified);

        assert_eq!(
            candidates.keys,
            vec!["alpha::parse".to_owned(), "zeta::from".to_owned()],
        );
        assert_eq!(
            candidates.pairs,
            vec![
                PairedCandidate { i: 2, j: 3, key: 0 },
                PairedCandidate { i: 0, j: 1, key: 1 },
            ],
        );
    }
}

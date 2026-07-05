//! Doc-comment similarity between a pair of functions.
//!
//! Doc text is a natural-language statement of *intent*, orthogonal to
//! the structural body/signature scores: two functions can share a doc
//! vocabulary while their implementations diverge, and vice versa. The
//! overlap is reported as a diagnostic component only — it does not
//! feed the blended similarity threshold — so agents can use it to
//! tell "same intent, same shape" clones apart from structural
//! coincidences.

use std::collections::HashSet;

/// English function-words that carry no intent signal in doc prose.
/// Kept deliberately small: over-aggressive stopword lists start eating
/// domain vocabulary, and Jaccard only needs the highest-frequency
/// glue words removed to stop unrelated docs from overlapping.
const DOC_STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "if", "in", "into", "is", "it",
    "its", "of", "on", "or", "s", "so", "that", "the", "this", "to", "when", "with",
];

/// Jaccard overlap of the two docs' word vocabularies, or `None` unless
/// both sides actually carry doc text. Word extraction lowercases and
/// splits on non-alphanumeric boundaries, so `UserId` / `user_id` /
/// "user id" all contribute the same tokens as identifier-style text.
pub(super) fn doc_overlap(a: Option<&str>, b: Option<&str>) -> Option<f64> {
    let (a, b) = (doc_word_set(a?), doc_word_set(b?));
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let intersection = a.intersection(&b).count();
    let union = a.union(&b).count();
    Some(intersection as f64 / union as f64)
}

fn doc_word_set(doc: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in doc.split(|ch: char| !ch.is_alphanumeric()) {
        for token in split_camel_case(word) {
            if !DOC_STOPWORDS.contains(&token.as_str()) {
                out.insert(token);
            }
        }
    }
    out
}

/// Split one whitespace/punctuation-free word at lowercase→uppercase
/// boundaries and lowercase the pieces, so a type name mentioned in
/// prose (`UserId`) matches its snake_case spelling (`user_id`). The
/// boundary rule mirrors the signature tokenizer in `lens-rust`.
fn split_camel_case(word: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut prev_is_lower_or_digit = false;
    for ch in word.chars() {
        if ch.is_uppercase() && prev_is_lower_or_digit && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }
        current.extend(ch.to_lowercase());
        prev_is_lower_or_digit = ch.is_lowercase() || ch.is_ascii_digit();
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::both_missing(None, None)]
    #[case::left_missing(None, Some("Parses the config."))]
    #[case::right_missing(Some("Parses the config."), None)]
    #[case::stopwords_only(Some("of the and"), Some("of the and"))]
    fn doc_overlap_requires_doc_words_on_both_sides(
        #[case] a: Option<&str>,
        #[case] b: Option<&str>,
    ) {
        assert_eq!(doc_overlap(a, b), None);
    }

    #[test]
    fn identical_docs_score_full_overlap() {
        let doc = Some("Validate the user id before persisting.");
        assert_eq!(doc_overlap(doc, doc), Some(1.0));
    }

    #[test]
    fn overlap_is_case_and_separator_insensitive() {
        let overlap = doc_overlap(Some("Returns the UserId."), Some("returns the user_id"));
        // "returns" and {"user", "id"} all match across casing styles.
        assert_eq!(overlap, Some(1.0));
    }

    #[test]
    fn unrelated_docs_score_low_despite_shared_glue_words() {
        let overlap = doc_overlap(
            Some("Reads the cache entry for a key."),
            Some("Formats the error message for display."),
        )
        .unwrap();
        assert!(overlap < 0.2, "got {overlap}");
    }

    #[test]
    fn partially_shared_vocabulary_scores_between_zero_and_one() {
        let overlap = doc_overlap(
            Some("Validate user id"),
            Some("Validate order id before saving"),
        )
        .unwrap();
        // {validate, id} shared; {user} vs {order, before, saving} unique.
        assert!((overlap - 2.0 / 6.0).abs() < 1e-9, "got {overlap}");
    }
}

//! Bare-name references outside a set of already-accounted spans.
//!
//! `single_use`, `single_impl`, and `parameters` each ask the same
//! textual question after the call graph has answered the structural
//! one: does a target's name appear anywhere its known definition and
//! callers do not explain? This index answers it per token, so each
//! analyzer keeps only its own tally.

use std::collections::HashMap;

/// A `(file, start_line, end_line)` span whose mentions of a target's
/// name are already accounted for.
pub(super) type AllowedSpan = (String, usize, usize);

/// Target slots by bare name, and each file's allowed spans tagged with
/// the slot they belong to.
pub(super) struct SpanReferenceIndex<'a> {
    slots_by_name: HashMap<&'a str, Vec<usize>>,
    allowed_by_file: HashMap<&'a str, Vec<(usize, usize, usize)>>,
}

impl<'a> SpanReferenceIndex<'a> {
    /// Index `(slot, name, allowed spans)` triples. Slots are the
    /// caller's own indices and need not be contiguous.
    pub(super) fn new(
        targets: impl IntoIterator<Item = (usize, &'a str, &'a [AllowedSpan])>,
    ) -> Self {
        let mut slots_by_name: HashMap<&str, Vec<usize>> = HashMap::new();
        let mut allowed_by_file: HashMap<&str, Vec<(usize, usize, usize)>> = HashMap::new();
        for (slot, name, allowed) in targets {
            slots_by_name.entry(name).or_default().push(slot);
            for (file, start, end) in allowed {
                allowed_by_file
                    .entry(file.as_str())
                    .or_default()
                    .push((*start, *end, slot));
            }
        }
        Self {
            slots_by_name,
            allowed_by_file,
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.slots_by_name.is_empty()
    }

    /// The view of the index for one scanned file.
    pub(super) fn file(&self, file: &str) -> FileSpans<'_> {
        FileSpans {
            index: self,
            allowed: self.allowed_by_file.get(file).map_or(&[], Vec::as_slice),
        }
    }
}

/// [`SpanReferenceIndex`] narrowed to the allowed spans of one file.
pub(super) struct FileSpans<'i> {
    index: &'i SpanReferenceIndex<'i>,
    allowed: &'i [(usize, usize, usize)],
}

impl FileSpans<'_> {
    /// Slots named `token` whose allowed spans in this file do not cover
    /// `line_no`: each is one unaccounted reference.
    pub(super) fn unaccounted(
        &self,
        token: &str,
        line_no: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        self.index
            .slots_by_name
            .get(token)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |&slot| {
                !self
                    .allowed
                    .iter()
                    .any(|&(start, end, s)| s == slot && (start..=end).contains(&line_no))
            })
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{AllowedSpan, SpanReferenceIndex};

    fn span(file: &str, start: usize, end: usize) -> AllowedSpan {
        (file.to_owned(), start, end)
    }

    #[rstest]
    #[case::inside_own_span("a.rs", "foo", 5, vec![])]
    #[case::span_edges_are_inclusive("a.rs", "foo", 10, vec![])]
    #[case::outside_own_span("a.rs", "foo", 11, vec![0])]
    #[case::other_file("b.rs", "foo", 5, vec![0])]
    #[case::other_targets_span_does_not_count("a.rs", "bar", 5, vec![1])]
    #[case::unknown_name("a.rs", "baz", 5, vec![])]
    #[case::shared_name_splits_by_slot("c.rs", "dup", 2, vec![3])]
    fn unaccounted_slots(
        #[case] file: &str,
        #[case] token: &str,
        #[case] line_no: usize,
        #[case] expected: Vec<usize>,
    ) {
        let foo = [span("a.rs", 1, 10)];
        let bar = [span("b.rs", 1, 10)];
        let dup_a = [span("c.rs", 1, 3)];
        let dup_b = [span("d.rs", 1, 3)];
        let index = SpanReferenceIndex::new([
            (0, "foo", &foo[..]),
            (1, "bar", &bar[..]),
            (2, "dup", &dup_a[..]),
            (3, "dup", &dup_b[..]),
        ]);
        let got: Vec<usize> = index.file(file).unaccounted(token, line_no).collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn empty_without_targets() {
        assert!(SpanReferenceIndex::new([]).is_empty());
    }
}

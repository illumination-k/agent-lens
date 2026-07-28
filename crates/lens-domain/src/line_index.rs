//! Byte offset → 1-based line number mapping.
//!
//! Every parser `agent-lens` wraps reports positions as byte offsets
//! (`oxc_span::Span` for TypeScript / JavaScript, `ruff_text_size::TextRange`
//! for Python), while the rest of the tool works in 1-based inclusive line
//! numbers. The conversion is a binary search over the line-start table, and
//! it is the same search regardless of which parser produced the offset, so
//! it lives here rather than being re-copied into each adapter.
//!
//! Offsets are `u32` because both upstream span types are `u32`-backed;
//! adapters that carry `usize` offsets should narrow at the call site
//! (`TextSize::to_u32`) rather than widen the table.

/// Maps byte offsets in a source string to 1-based line numbers.
///
/// Build one per source file and reuse it for every span in that file:
/// construction is linear in the source length, each lookup is logarithmic
/// in the line count.
#[derive(Debug)]
pub struct LineIndex {
    /// Byte offset where each line starts. `starts[0] == 0` for the
    /// first line; `starts[i]` is the offset of the first byte after the
    /// `i-1`th newline. Always non-empty, so every lookup lands on a line.
    starts: Vec<u32>,
}

impl LineIndex {
    /// Index the line starts of `source`.
    pub fn new(source: &str) -> Self {
        // ~32 bytes per line is a rough fit for source code; over- or
        // under-shooting only costs a realloc.
        let mut starts = Vec::with_capacity(source.len() / 32 + 1);
        starts.push(0u32);
        for (i, b) in source.bytes().enumerate() {
            if b == b'\n' {
                let next = u32::try_from(i + 1).unwrap_or(u32::MAX);
                starts.push(next);
            }
        }
        Self { starts }
    }

    /// 1-based line number of the byte at `offset`.
    ///
    /// A newline byte belongs to the line it terminates. Offsets past the
    /// end of the source map to the last line, so callers do not have to
    /// bounds-check a span end against the file length.
    pub fn line(&self, offset: u32) -> usize {
        match self.starts.binary_search(&offset) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use rstest::rstest;

    #[rstest]
    #[case("hello\nworld\n", 0, 1)]
    #[case("hello\nworld\n", 4, 1)]
    // The `\n` at offset 5 ends line 1; offset 6 starts line 2.
    #[case("hello\nworld\n", 5, 1)]
    #[case("hello\nworld\n", 6, 2)]
    #[case("a\nb\nc\n", 0, 1)]
    #[case("a\nb\nc\n", 2, 2)]
    #[case("a\nb\nc\n", 4, 3)]
    // Offsets past the end of the source clamp to the last line.
    #[case("a\nb", 99, 2)]
    #[case("", 0, 1)]
    #[case("", 99, 1)]
    // Multi-byte characters are counted in bytes, not chars.
    #[case("日本語\nx", 9, 1)]
    #[case("日本語\nx", 10, 2)]
    fn line_maps_offsets_to_one_based_lines(
        #[case] source: &str,
        #[case] offset: u32,
        #[case] expected: usize,
    ) {
        assert_eq!(LineIndex::new(source).line(offset), expected);
    }

    /// Reference implementation: count the newlines strictly before
    /// `offset`. Deliberately naive, so it disagrees with the binary
    /// search if the search ever regresses.
    fn line_by_scan(source: &str, offset: u32) -> usize {
        let stop = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(source.len());
        1 + source.as_bytes()[..stop]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
    }

    proptest! {
        #[test]
        fn line_agrees_with_a_linear_scan(source in ".{0,200}", offset in 0u32..256) {
            prop_assert_eq!(
                LineIndex::new(&source).line(offset),
                line_by_scan(&source, offset),
            );
        }

        // The mapping must never go backwards as the offset grows: a
        // span's end line is always >= its start line.
        #[test]
        fn line_is_monotonic_in_offset(source in ".{0,200}") {
            let index = LineIndex::new(&source);
            let mut previous = 0;
            for offset in 0..=u32::try_from(source.len()).unwrap_or(u32::MAX) {
                let line = index.line(offset);
                prop_assert!(line >= previous, "line went backwards at offset {offset}");
                previous = line;
            }
        }
    }
}

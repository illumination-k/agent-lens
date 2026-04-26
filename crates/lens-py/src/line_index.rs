//! Utility for mapping byte offsets into 1-based line numbers.
//!
//! `ruff_text_size::TextRange` carries byte offsets, but the rest of
//! `agent-lens` works in 1-based inclusive line numbers. We compute a
//! `LineIndex` once per source file and reuse it for every span we need
//! to map. Mirrors the same helper shipped in `lens-ts` so callers in the
//! domain layer don't have to know which adapter produced a span.

/// Maps byte offsets in a source string to 1-based line numbers.
pub(crate) struct LineIndex {
    /// Byte offset where each line starts. `starts[0] == 0` for the
    /// first line; `starts[i]` is the offset of the first byte after the
    /// `i-1`th newline.
    starts: Vec<u32>,
}

impl LineIndex {
    pub(crate) fn new(source: &str) -> Self {
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

    /// 1-based line number of the byte at `offset`. Offsets past the end
    /// of the source map to the last line.
    pub(crate) fn line_of(&self, offset: usize) -> usize {
        let target = u32::try_from(offset).unwrap_or(u32::MAX);
        match self.starts.binary_search(&target) {
            Ok(idx) => idx + 1,
            Err(idx) => idx,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_line_is_one() {
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(4), 1);
    }

    #[test]
    fn newline_byte_belongs_to_originating_line() {
        // The `\n` at offset 5 ends line 1; offset 6 starts line 2.
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.line_of(5), 1);
        assert_eq!(idx.line_of(6), 2);
    }

    #[test]
    fn third_line_after_two_newlines() {
        let idx = LineIndex::new("a\nb\nc\n");
        assert_eq!(idx.line_of(0), 1);
        assert_eq!(idx.line_of(2), 2);
        assert_eq!(idx.line_of(4), 3);
    }

    #[test]
    fn offsets_past_end_map_to_last_line() {
        let idx = LineIndex::new("a\nb");
        assert_eq!(idx.line_of(99), 2);
    }

    #[test]
    fn empty_source_maps_to_line_one() {
        let idx = LineIndex::new("");
        assert_eq!(idx.line_of(0), 1);
    }
}

//! Utility for mapping byte offsets into 1-based line numbers.
//!
//! `oxc_span::Span` carries byte offsets, but the rest of `agent-lens`
//! works in 1-based inclusive line numbers. We compute a `LineIndex`
//! once per source file and reuse it for every span we need to map.

/// Maps byte offsets in a source string to 1-based line numbers.
pub struct LineIndex {
    /// Byte offset where each line starts. `starts[0] == 0` for the
    /// first line; `starts[i]` is the offset of the first byte after the
    /// `i-1`th newline.
    starts: Vec<u32>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0u32];
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

    #[test]
    fn first_line_is_one() {
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.line(0), 1);
        assert_eq!(idx.line(4), 1);
    }

    #[test]
    fn newline_byte_belongs_to_originating_line() {
        // The `\n` at offset 5 ends line 1; offset 6 starts line 2.
        let idx = LineIndex::new("hello\nworld\n");
        assert_eq!(idx.line(5), 1);
        assert_eq!(idx.line(6), 2);
    }

    #[test]
    fn third_line_after_two_newlines() {
        let idx = LineIndex::new("a\nb\nc\n");
        assert_eq!(idx.line(0), 1);
        assert_eq!(idx.line(2), 2);
        assert_eq!(idx.line(4), 3);
    }

    #[test]
    fn offsets_past_end_map_to_last_line() {
        let idx = LineIndex::new("a\nb");
        assert_eq!(idx.line(99), 2);
    }

    #[test]
    fn empty_source_maps_to_line_one() {
        let idx = LineIndex::new("");
        assert_eq!(idx.line(0), 1);
    }
}

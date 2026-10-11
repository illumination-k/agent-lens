//! Which source lines carry code, as opposed to blank lines and comments.
//!
//! `similarity --target blocks` cuts windows whose comparison tree is too
//! sparse for the lines they span ([`crate::MIN_WINDOW_NODES_PER_LINE`]).
//! Comments never reach the tree, so a well-commented window would be
//! charged for lines the comparison cannot see and dropped as if it were a
//! declaration run. Counting only code lines keeps the floor about what it
//! guards against: source the tree under-represents.
//!
//! The scan is lexical, not a parse: it tracks line and block comments and
//! the string literals that could hide a comment marker, which is all the
//! question needs. Lines inside a multi-line string count as code, since a
//! string the tree collapses to one leaf is exactly what the density floor
//! exists to catch. Constructs it does not model (a TypeScript regex
//! literal containing `//`, say) can only misjudge the rest of that one
//! line.

/// The comment and string syntax of a language family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSyntax {
    /// `//`, nestable `/* */`, `'` as a char literal or a lifetime, and
    /// `r#"…"#` raw strings.
    Rust,
    /// `//` and `/* */`, with `'`, `"` and backtick strings: TypeScript,
    /// JavaScript and Go.
    CLike,
    /// `#` comments and `'` / `"` strings, single or triple quoted.
    Python,
}

/// The 1-based lines of a source file that hold no code: blank, or
/// nothing but comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NonCodeLines {
    /// Ascending.
    lines: Vec<usize>,
}

impl NonCodeLines {
    /// Scan `source` for blank and comment-only lines.
    ///
    /// # Examples
    ///
    /// ```
    /// use lens_domain::{CommentSyntax, NonCodeLines};
    ///
    /// let src = "let a = 1;\n\n// note\nlet b = \"// not a comment\";\n";
    /// let non_code = NonCodeLines::scan(src, CommentSyntax::Rust);
    /// assert_eq!(non_code.count_within(1, 4), 2);
    /// ```
    pub fn scan(source: &str, syntax: CommentSyntax) -> Self {
        let mut scanner = Scanner {
            syntax,
            state: State::Code,
        };
        let lines = source
            .lines()
            .enumerate()
            .filter(|(_, line)| !scanner.line_has_code(line))
            .map(|(idx, _)| idx + 1)
            .collect();
        Self { lines }
    }

    /// How many of the lines `start..=end` (1-based) hold no code.
    pub fn count_within(&self, start: usize, end: usize) -> usize {
        let from = self.lines.partition_point(|&line| line < start);
        let to = self.lines.partition_point(|&line| line <= end);
        to.saturating_sub(from)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    /// Inside a block comment, at this nesting depth.
    BlockComment(usize),
    /// Inside a string literal.
    Str(StrKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StrKind {
    quote: char,
    /// Python `'''` / `"""`.
    triple: bool,
    /// Rust raw string: the number of `#` closing it; no escapes.
    raw_hashes: Option<usize>,
    /// Go backtick strings take no escapes.
    escapes: bool,
}

struct Scanner {
    syntax: CommentSyntax,
    state: State,
}

impl Scanner {
    /// Advance over one line, carrying block-comment and string state
    /// into the next; true when any of it was code.
    fn line_has_code(&mut self, line: &str) -> bool {
        let chars: Vec<char> = line.chars().collect();
        let mut has_code = matches!(self.state, State::Str(_));
        let mut i = 0;
        while i < chars.len() {
            i = match self.state {
                State::BlockComment(depth) => self.in_block_comment(&chars, i, depth),
                State::Str(kind) => self.in_string(&chars, i, kind),
                State::Code => {
                    let Some(next) = self.in_code(&chars, i, &mut has_code) else {
                        break;
                    };
                    next
                }
            };
        }
        has_code
    }

    /// Step over code at `chars[i]`. `None` when the rest of the line is
    /// a line comment.
    fn in_code(&mut self, chars: &[char], i: usize, has_code: &mut bool) -> Option<usize> {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        if c.is_whitespace() {
            return Some(i + 1);
        }
        match (self.syntax, c, next) {
            (CommentSyntax::Python, '#', _) => return None,
            (CommentSyntax::Rust | CommentSyntax::CLike, '/', Some('/')) => return None,
            (CommentSyntax::Rust | CommentSyntax::CLike, '/', Some('*')) => {
                self.state = State::BlockComment(1);
                return Some(i + 2);
            }
            _ => {}
        }
        *has_code = true;
        Some(self.open_string(chars, i).unwrap_or(i + 1))
    }

    /// Enter a string literal starting at `chars[i]`, returning the index
    /// after its opening delimiter, or `None` when no string starts here.
    fn open_string(&mut self, chars: &[char], i: usize) -> Option<usize> {
        let c = chars[i];
        let plain = |quote| StrKind {
            quote,
            triple: false,
            raw_hashes: None,
            escapes: true,
        };
        let (kind, width) = match (self.syntax, c) {
            (CommentSyntax::Rust, 'r') => rust_raw_string(chars, i)?,
            (CommentSyntax::Rust, '\'') => {
                // A char literal closes within three characters ('a',
                // '\n'); anything else is a lifetime or a label.
                let is_char = chars.get(i + 1) == Some(&'\\') || chars.get(i + 2) == Some(&'\'');
                if !is_char {
                    return None;
                }
                (plain('\''), 1)
            }
            (CommentSyntax::Rust, '"') => (plain('"'), 1),
            (CommentSyntax::CLike, '"' | '\'') => (plain(c), 1),
            (CommentSyntax::CLike, '`') => (
                StrKind {
                    // TS templates take escapes, Go raw strings do not;
                    // `\` before a backtick only matters to the former.
                    escapes: true,
                    ..plain('`')
                },
                1,
            ),
            (CommentSyntax::Python, '"' | '\'') => {
                let triple = chars.get(i + 1) == Some(&c) && chars.get(i + 2) == Some(&c);
                (StrKind { triple, ..plain(c) }, if triple { 3 } else { 1 })
            }
            _ => return None,
        };
        self.state = State::Str(kind);
        Some(i + width)
    }

    fn in_block_comment(&mut self, chars: &[char], i: usize, depth: usize) -> usize {
        let next = chars.get(i + 1).copied();
        match (chars[i], next) {
            ('*', Some('/')) => {
                self.state = if depth > 1 {
                    State::BlockComment(depth - 1)
                } else {
                    State::Code
                };
                i + 2
            }
            ('/', Some('*')) if self.syntax == CommentSyntax::Rust => {
                self.state = State::BlockComment(depth + 1);
                i + 2
            }
            _ => i + 1,
        }
    }

    fn in_string(&mut self, chars: &[char], i: usize, kind: StrKind) -> usize {
        let c = chars[i];
        if c == '\\' && kind.escapes && kind.raw_hashes.is_none() {
            return i + 2;
        }
        if c != kind.quote {
            return i + 1;
        }
        let closing = if kind.triple {
            3
        } else {
            1 + kind.raw_hashes.unwrap_or(0)
        };
        let closes = (1..closing).all(|k| {
            let expected = if kind.triple { kind.quote } else { '#' };
            chars.get(i + k) == Some(&expected)
        });
        if !closes {
            return i + 1;
        }
        self.state = State::Code;
        i + closing
    }
}

/// `r"…"`, `r#"…"#` (also after a `b` / `c` prefix) starting at the `r`
/// in `chars[i]`: the string kind and the opening delimiter's width.
fn rust_raw_string(chars: &[char], i: usize) -> Option<(StrKind, usize)> {
    let prev = i.checked_sub(1).and_then(|p| chars.get(p));
    if prev.is_some_and(|&p| (p.is_alphanumeric() || p == '_') && p != 'b' && p != 'c') {
        return None;
    }
    let hashes = chars[i + 1..].iter().take_while(|&&c| c == '#').count();
    if chars.get(i + 1 + hashes) != Some(&'"') {
        return None;
    }
    let kind = StrKind {
        quote: '"',
        triple: false,
        raw_hashes: Some(hashes),
        escapes: false,
    };
    Some((kind, hashes + 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn non_code(source: &str, syntax: CommentSyntax) -> Vec<usize> {
        let scanned = NonCodeLines::scan(source, syntax);
        scanned.lines
    }

    #[rstest]
    #[case::blank_and_line_comments(
        CommentSyntax::Rust,
        "let a = 1;\n\n   // note\nlet b = 2; // trailing\n",
        vec![2, 3]
    )]
    #[case::nested_block_comment(
        CommentSyntax::Rust,
        "/* outer\n /* inner */\n still comment */\nlet a = 1;\n",
        vec![1, 2, 3]
    )]
    #[case::comment_marker_inside_a_string(
        CommentSyntax::Rust,
        "let url = \"http://x\";\nlet s = \"/* not a comment\";\nlet t = 1;\n",
        vec![]
    )]
    #[case::raw_string_with_quotes(
        CommentSyntax::Rust,
        "let s = r#\"say \"hi\" // x\n\n\"#;\n// done\n",
        vec![4]
    )]
    #[case::lifetime_is_not_a_char_literal(
        CommentSyntax::Rust,
        "fn f<'a>(x: &'a str) {}\n// note\nlet c = '\"';\n// after\n",
        vec![2, 4]
    )]
    #[case::ts_template_spans_lines(
        CommentSyntax::CLike,
        "const t = `\n// inside\n`;\n// outside\n",
        vec![4]
    )]
    #[case::c_block_comments_do_not_nest(
        CommentSyntax::CLike,
        "/* a /* b */\nx := 1\n",
        vec![1]
    )]
    #[case::python_hash_and_triple_quotes(
        CommentSyntax::Python,
        "x = 1  # trailing\n# note\ns = \"\"\"\n# inside\n\"\"\"\ny = '#'\n",
        vec![2]
    )]
    fn scan_marks_blank_and_comment_only_lines(
        #[case] syntax: CommentSyntax,
        #[case] source: &str,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(non_code(source, syntax), expected);
    }

    #[rstest]
    #[case(1, 5, 2)]
    #[case(3, 3, 1)]
    #[case(4, 5, 0)]
    #[case(6, 9, 1)]
    fn count_within_is_inclusive(
        #[case] start: usize,
        #[case] end: usize,
        #[case] expected: usize,
    ) {
        let lines = NonCodeLines {
            lines: vec![2, 3, 7],
        };
        assert_eq!(lines.count_within(start, end), expected);
    }
}

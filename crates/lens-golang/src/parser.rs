//! tree-sitter-based implementation of [`lens_domain::LanguageParser`]
//! for Go.
//!
//! Functions are extracted from:
//!
//! * `function_declaration` — top-level `func name(...) { ... }`,
//! * `method_declaration` — `func (r Receiver) name(...) { ... }`,
//!   qualified as `Receiver::name` (with `*` stripped from pointer
//!   receivers).
//!
//! Closures (`func_literal`) are deliberately left out: their containing
//! function is the unit of analysis, mirroring how `lens-rust` keeps
//! closures inside their parent fn and `lens-ts` skips inner functions.
//! Interface method elements have no body and are therefore not
//! function-shaped at all.

use lens_domain::{
    FunctionDef, FunctionSignature, LanguageParseError, LanguageParser, ReceiverShape, TreeNode,
    identifier_tokens, qualify as qualify_name,
};
use tree_sitter::{Node, Parser};

use crate::attrs::name_looks_like_test_function;
use crate::node_text::{node_str, node_text_or_empty};
use crate::walk::{FnSite, walk_top_level_fns};

/// Tree-sitter-backed Go parser.
///
/// The struct is stateless from the caller's perspective; tree-sitter's
/// parser is created on demand inside [`LanguageParser::parse`] /
/// [`LanguageParser::extract_functions`]. This keeps `GoParser: Send +
/// Sync` trivially and matches how the other language adapters expose
/// their parsers.
#[derive(Debug, Default, Clone, Copy)]
pub struct GoParser;

impl GoParser {
    pub fn new() -> Self {
        Self
    }
}

/// Parse failures surfaced by [`GoParser`].
#[derive(Debug, thiserror::Error)]
pub enum GoParseError {
    /// The bundled tree-sitter Go grammar was rejected by the runtime.
    /// Should only fire if the `tree-sitter` and `tree-sitter-go`
    /// crate versions go out of sync at the ABI level.
    #[error("failed to load tree-sitter Go grammar: {0}")]
    Grammar(#[source] tree_sitter::LanguageError),
    /// `tree-sitter` returned `None` from `parse`. In practice this only
    /// happens if the parser is mis-configured (no language set, or the
    /// previous parse was cancelled); we surface it as its own variant
    /// rather than swallowing it.
    #[error("tree-sitter Go parser returned no tree")]
    NoTree,
    /// The tree-sitter parse produced one or more `ERROR` / `MISSING`
    /// nodes. Tree-sitter is error-tolerant — it always builds a tree —
    /// so this surfaces input that wouldn't compile under `gofmt`.
    #[error("failed to parse Go source: tree contains parse errors")]
    Syntax,
}

impl LanguageParser for GoParser {
    fn language(&self) -> &'static str {
        "go"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let tree =
            parse_tree(source).map_err(|err| LanguageParseError::new(self.language(), err))?;
        let root = tree.root_node();
        let bytes = source.as_bytes();
        Ok(build_tree(root, bytes, /* is_root = */ true))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        extract_with(source).map_err(|err| LanguageParseError::new(self.language(), err))
    }
}

fn extract_with(source: &str) -> Result<Vec<FunctionDef>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        out.push(function_def_from(&site, source.as_bytes()));
    });
    Ok(out)
}

pub(crate) fn parse_tree(source: &str) -> Result<tree_sitter::Tree, GoParseError> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_go::LANGUAGE.into())
        .map_err(GoParseError::Grammar)?;
    let tree = parser.parse(source, None).ok_or(GoParseError::NoTree)?;
    if tree.root_node().has_error() {
        return Err(GoParseError::Syntax);
    }
    Ok(tree)
}

/// Lower one walked declaration into a [`FunctionDef`].
fn function_def_from(site: &FnSite<'_, '_>, source: &[u8]) -> FunctionDef {
    let owner = site.owner.as_deref();
    let is_test = owner.is_none() && name_looks_like_test_function(site.name);
    let node = site.node;
    FunctionDef {
        name: qualify_name(owner, site.name),
        start_line: node.start_position().row + 1,
        end_line: node.end_position().row + 1,
        is_test,
        signature: Some(signature_info(node, source, site.name)),
        doc: doc_comment_text(node, source),
        implements: None,
        tree: build_tree(site.body, source, /* is_root = */ true),
    }
}

/// Project a `function_declaration` / `method_declaration` into the
/// language-neutral [`FunctionSignature`] used by signature-aware
/// similarity. Parameter and return types are captured as their raw
/// source text (`[]byte`, `*T`, …) — enough for the within-language
/// token overlap the scorer computes.
fn signature_info(node: Node<'_>, source: &[u8], raw_name: &str) -> FunctionSignature {
    let mut parameter_names = Vec::new();
    let mut parameter_type_paths = Vec::new();
    let mut parameter_count = 0usize;

    if let Some(params) = node.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for decl in params.named_children(&mut cursor) {
            if !matches!(
                decl.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            ) {
                continue;
            }
            let names = declaration_names(decl, source);
            // An unnamed parameter (`func f(int)`) still occupies one slot.
            parameter_count += names.len().max(1);
            if let Some(ty) = decl.child_by_field_name("type")
                && let Some(text) = node_str(ty, source)
            {
                parameter_type_paths.push(text.to_owned());
            }
            parameter_names.extend(names);
        }
    }

    let mut return_type_paths = Vec::new();
    if let Some(result) = node.child_by_field_name("result") {
        collect_result_types(result, source, &mut return_type_paths);
    }

    FunctionSignature {
        name_tokens: identifier_tokens(raw_name),
        parameter_count,
        parameter_names,
        parameter_type_paths,
        return_type_paths,
        generics: collect_type_parameters(node, source),
        receiver: receiver_shape(node),
    }
}

/// The `name:` identifiers of a parameter / spec declaration, in order.
/// `func f(a, b int)` yields `["a", "b"]`; an unnamed parameter yields
/// an empty list.
pub(crate) fn declaration_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut cursor = node.walk();
    if !cursor.goto_first_child() {
        return names;
    }
    loop {
        if cursor.field_name() == Some("name")
            && cursor.node().kind() == "identifier"
            && let Some(text) = node_str(cursor.node(), source)
        {
            names.push(text.to_owned());
        }
        if !cursor.goto_next_sibling() {
            break;
        }
    }
    names
}

/// Per-slot parameter names of a `parameter_list`, with grouped
/// declarations expanded — `(a, b int)` yields two slots — and unnamed
/// parameters holding `None`. A variadic declaration is one slot. This
/// slot expansion is what makes two Go signatures comparable by arity:
/// `Do(a, b int)` and `Do(int, int)` both declare two parameters.
pub(crate) fn parameter_slot_names(params: Node<'_>, source: &[u8]) -> Vec<Option<String>> {
    let mut slots = Vec::new();
    let mut cursor = params.walk();
    for decl in params.named_children(&mut cursor) {
        if !matches!(
            decl.kind(),
            "parameter_declaration" | "variadic_parameter_declaration"
        ) {
            continue;
        }
        let names = declaration_names(decl, source);
        if names.is_empty() {
            slots.push(None);
        } else {
            slots.extend(names.into_iter().map(Some));
        }
    }
    slots
}

/// Collect return type texts from a `result:` node, which is either a
/// parenthesized `parameter_list` (`(int, error)`) or a single bare type.
fn collect_result_types(result: Node<'_>, source: &[u8], out: &mut Vec<String>) {
    if result.kind() == "parameter_list" {
        let mut cursor = result.walk();
        for decl in result.named_children(&mut cursor) {
            if let Some(ty) = decl.child_by_field_name("type")
                && let Some(text) = node_str(ty, source)
            {
                out.push(text.to_owned());
            }
        }
    } else if let Some(text) = node_str(result, source) {
        out.push(text.to_owned());
    }
}

/// Collect generic type-parameter declarations (`[T any]`), if any, as
/// their raw text.
pub(crate) fn collect_type_parameters(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let Some(params) = node.child_by_field_name("type_parameters") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut cursor = params.walk();
    for decl in params.named_children(&mut cursor) {
        if decl.kind() == "type_parameter_declaration"
            && let Some(text) = node_str(decl, source)
        {
            out.push(text.to_owned());
        }
    }
    out
}

/// Map a method receiver to a [`ReceiverShape`]: a pointer receiver
/// (`(s *S)`) is a reference, a value receiver (`(s S)`) is by value,
/// and a free function has no receiver.
fn receiver_shape(node: Node<'_>) -> ReceiverShape {
    let Some(receiver) = node.child_by_field_name("receiver") else {
        return ReceiverShape::None;
    };
    let mut cursor = receiver.walk();
    for decl in receiver.named_children(&mut cursor) {
        if decl.kind() == "parameter_declaration"
            && let Some(ty) = decl.child_by_field_name("type")
        {
            return if ty.kind() == "pointer_type" {
                ReceiverShape::Ref
            } else {
                ReceiverShape::Value
            };
        }
    }
    ReceiverShape::None
}

/// Godoc-style doc comment: the run of `comment` siblings immediately
/// above the declaration, with no blank line between the last comment
/// and the declaration (or between the comments themselves). Comment
/// markers (`//`, `/* */`) are stripped per line. Returns `None` when
/// there is no adjacent comment or it is blank.
pub(crate) fn doc_comment_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    let doc = adjacent_comments(node, source)
        .into_iter()
        .map(strip_comment_markers)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    (!doc.is_empty()).then_some(doc)
}

/// The raw text of the `comment` siblings forming the declaration's doc
/// block, in source order. Shared by [`doc_comment_text`], which reads
/// them as prose, and [`directive_names`], which reads them as compiler
/// directives — the two disagree about markers and whitespace, so the
/// split happens after the walk rather than inside it.
fn adjacent_comments<'a>(node: Node<'_>, source: &'a [u8]) -> Vec<&'a str> {
    let mut comments: Vec<&'a str> = Vec::new();
    let mut expected_row = node.start_position().row;
    let mut prev = node.prev_sibling();
    while let Some(sibling) = prev {
        if sibling.kind() != "comment" || sibling.end_position().row + 1 != expected_row {
            break;
        }
        let Some(text) = node_str(sibling, source) else {
            break;
        };
        comments.push(text);
        expected_row = sibling.start_position().row;
        prev = sibling.prev_sibling();
    }
    comments.reverse();
    comments
}

/// Compiler directives written above the declaration, named without
/// their arguments: `//go:linkname local remote` yields `go:linkname`,
/// the cgo `//export Name` yields `export`.
///
/// Go has no attribute syntax, so a directive is a comment the toolchain
/// happens to read — and the spelling is exact: no space after `//`, the
/// directive name runs to the first space. Prose that merely starts with
/// the word "export" is not one, hence the `//export ` prefix rather
/// than a search.
pub(crate) fn directive_names(node: Node<'_>, source: &[u8]) -> Vec<String> {
    adjacent_comments(node, source)
        .into_iter()
        .flat_map(str::lines)
        .filter_map(|line| {
            let line = line.trim_end();
            if let Some(rest) = line.strip_prefix("//go:") {
                let name = rest.split_whitespace().next().unwrap_or_default();
                return (!name.is_empty()).then(|| format!("go:{name}"));
            }
            line.strip_prefix("//export ")
                .is_some_and(|name| !name.trim().is_empty())
                .then(|| "export".to_owned())
        })
        .collect()
}

/// Strip `//` / `/* */` markers from one comment node's text, trimming
/// each line so multi-line block comments fold to their prose.
fn strip_comment_markers(text: &str) -> String {
    let body = text
        .strip_prefix("/*")
        .and_then(|rest| rest.strip_suffix("*/"))
        .unwrap_or_else(|| text.strip_prefix("//").unwrap_or(text));
    body.lines()
        .map(str::trim)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Resolve the user-visible name of a `function_declaration` or
/// `method_declaration`. Free functions use the `name: identifier`
/// field; methods use `name: field_identifier`.
pub(crate) fn function_name_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")
        .and_then(|n| node_str(n, source))
}

/// Walk the receiver `parameter_list` of a `method_declaration` to
/// recover the receiver type's identifier. Pointer receivers
/// (`func (s *Foo) ...`) and value receivers (`func (s Foo) ...`) both
/// fold to `"Foo"`; named-but-unused (`func (Foo) ...`) and generic
/// (`func (s *Foo[T]) ...`) receivers are handled the same way.
///
/// Returns `None` for shapes the grammar accepts that don't carry a
/// recognisable type identifier (e.g. partial parses); the caller falls
/// back to an unqualified name in that case rather than dropping the
/// method.
pub(crate) fn method_receiver_type(node: Node<'_>, source: &[u8]) -> Option<String> {
    let receiver = node.child_by_field_name("receiver")?;
    let mut cursor = receiver.walk();
    for child in receiver.named_children(&mut cursor) {
        if child.kind() == "parameter_declaration"
            && let Some(type_node) = child.child_by_field_name("type")
            && let Some(text) = receiver_type_text(type_node, source)
        {
            return Some(text);
        }
    }
    None
}

fn receiver_type_text(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "type_identifier" => node_str(node, source).map(str::to_owned),
        "pointer_type" => {
            // `pointer_type` wraps the pointee as its sole named
            // child; the grammar doesn't expose it under a field name,
            // so walk named children rather than `child_by_field_name`.
            // Recurse so a pointer to a generic instance still resolves
            // to its outer type identifier.
            let mut cursor = node.walk();
            for child in node.named_children(&mut cursor) {
                if let Some(text) = receiver_type_text(child, source) {
                    return Some(text);
                }
            }
            None
        }
        "generic_type" => {
            // `Foo[T]` — the outer type identifier sits behind the
            // `type` field. Use it directly so we only return the
            // generic constructor's name, not the type argument.
            let inner = node.child_by_field_name("type")?;
            receiver_type_text(inner, source)
        }
        _ => None,
    }
}

/// Lower a function body's tree-sitter node into the generic
/// [`TreeNode`] used by APTED. Wrapper around [`build_tree`] that pins
/// `is_root = true` so the resulting tree's root carries the canonical
/// `"Block"` label.
pub(crate) fn function_body_tree(body: Node<'_>, source: &[u8]) -> TreeNode {
    build_tree(body, source, /* is_root = */ true)
}

/// Lower a single statement node into the subtree
/// [`function_body_tree`] nests under `Block`. Used by
/// `similarity --target blocks`, which compares runs of statements
/// rather than whole bodies; routing both through [`build_tree`] is what
/// keeps a window covering a whole body identical to that body's tree.
pub(crate) fn statement_tree(node: Node<'_>, source: &[u8]) -> TreeNode {
    build_tree(node, source, /* is_root = */ false)
}

/// Strip the surrounding quotes from a Go string literal.
///
/// Go has two string literal forms — interpreted strings (`"..."`) and
/// raw strings (`` `...` ``). Both are valid in `import` statements,
/// and their content (the import path) is identical between forms;
/// only escape handling differs, which doesn't matter for import
/// paths. Inputs that are too short to carry both delimiters or that
/// don't match a quote pair fall through unchanged so callers see the
/// original tokens rather than a silently-truncated value.
pub(crate) fn unquote_go_string_literal(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() < 2 {
        return trimmed.to_owned();
    }
    let bytes = trimmed.as_bytes();
    let first = bytes[0];
    let last = bytes[bytes.len() - 1];
    if (first == b'"' && last == b'"') || (first == b'`' && last == b'`') {
        trimmed[1..trimmed.len() - 1].to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Recursively lower a tree-sitter node into the generic [`TreeNode`]
/// used by APTED. Identifier-bearing nodes carry their text as `value`
/// so optional value-aware comparison can tell `Add` from `Mul`.
///
/// The body root is rewritten to the label `"Block"` so that Go
/// function bodies share the canonical name used by `lens-rust` /
/// `lens-py` / `lens-ts`. Without this, two structurally identical
/// bodies would still differ on the root label across languages —
/// fine for in-language similarity but inconsistent with the rest of
/// the workspace's tree shape.
fn build_tree(node: Node<'_>, source: &[u8], is_root: bool) -> TreeNode {
    let label = if is_root { "Block" } else { node.kind() };
    let value = node_value(node, source);
    let mut children = Vec::new();
    let mut cursor = node.walk();
    for child in node.named_children(&mut cursor) {
        children.push(build_tree(child, source, /* is_root = */ false));
    }
    TreeNode::with_children(label, value, children)
}

/// The `value` carried by a [`TreeNode`], used by value-aware APTED
/// comparison. Every other node kind already has no value, so an
/// unreadable identifier falling back to `""` degrades a comparison
/// rather than putting a blank name in a report — and [`node_str`] has
/// logged the read failure by the time we get here either way.
fn node_value(node: Node<'_>, source: &[u8]) -> String {
    match node.kind() {
        "identifier" | "field_identifier" | "package_identifier" | "type_identifier"
        | "label_name" => node_text_or_empty(node, source),
        "function_declaration" | "method_declaration" => {
            function_name_text(node, source).unwrap_or("").to_owned()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};
    use rstest::rstest;

    #[rstest]
    #[case::interpreted_string("\"hello\"", "hello")]
    #[case::raw_string("`world`", "world")]
    // `<` (not `<=` or `==`) is the right boundary: a length-2 string
    // (`""`) must still strip to the empty inner string, while a
    // single delimiter character is too short to round-trip and must
    // pass through unchanged.
    #[case::empty("", "")]
    #[case::single_quote_only("\"", "\"")]
    #[case::single_backtick_only("`", "`")]
    #[case::two_char_quoted_empty("\"\"", "")]
    #[case::two_char_raw_empty("``", "")]
    // Both endpoints must match the same delimiter style — without the
    // `&&` between (first quote AND last quote) and (first backtick AND
    // last backtick) inside an `||`, mismatched ends would silently
    // pass through `[1..len-1]` and chop off real characters.
    #[case::mismatched_quote_to_backtick("\"foo`", "\"foo`")]
    #[case::mismatched_backtick_to_quote("`foo\"", "`foo\"")]
    #[case::no_quotes("hello", "hello")]
    fn unquote_go_string_literal_handles_quoting_shapes(
        #[case] input: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(unquote_go_string_literal(input), expected);
    }

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = GoParser::new();
        parser.extract_functions(src).unwrap()
    }

    #[test]
    fn free_function_signature_captures_params_and_returns() {
        let src = "package p\nfunc Free(a int, b, c string) (int, error) { return 0, nil }\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.name_tokens, vec!["free".to_owned()]);
        assert_eq!(sig.parameter_count, 3);
        assert_eq!(
            sig.parameter_names,
            vec!["a".to_owned(), "b".to_owned(), "c".to_owned()],
        );
        assert!(sig.parameter_type_paths.contains(&"int".to_owned()));
        assert!(sig.parameter_type_paths.contains(&"string".to_owned()));
        assert_eq!(
            sig.return_type_paths,
            vec!["int".to_owned(), "error".to_owned()],
        );
        assert_eq!(sig.receiver, ReceiverShape::None);
    }

    #[test]
    fn pointer_method_signature_has_ref_receiver() {
        let src = "package p\ntype S struct{}\nfunc (s *S) Method(x []byte) *T { return nil }\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.receiver, ReceiverShape::Ref);
        assert_eq!(sig.parameter_count, 1);
        assert_eq!(sig.parameter_names, vec!["x".to_owned()]);
        assert!(sig.parameter_type_paths.contains(&"[]byte".to_owned()));
    }

    #[test]
    fn value_method_signature_has_value_receiver() {
        let src = "package p\ntype S struct{}\nfunc (s S) Read() int { return 0 }\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.receiver, ReceiverShape::Value);
    }

    #[test]
    fn generic_function_signature_records_type_parameters() {
        let src = "package p\nfunc Gen[T any](v T) T { return v }\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.parameter_count, 1);
        // The raw type-parameter declaration text is captured verbatim so
        // the content — not just presence — is pinned.
        assert_eq!(sig.generics, vec!["T any".to_owned()]);
    }

    #[test]
    fn unnamed_parameters_still_count() {
        let src = "package p\nfunc NoNames(int, string) bool { return false }\n";
        let funcs = parse_functions(src);
        let sig = funcs[0].signature.as_ref().expect("signature populated");
        assert_eq!(sig.parameter_count, 2);
        assert!(sig.parameter_names.is_empty());
    }

    #[rstest]
    #[case::single_line(
        "package p\n\n// Sum adds the values.\nfunc Sum(xs []int) int {\n\treturn 0\n}\n",
        Some("Sum adds the values.")
    )]
    #[case::multi_line(
        "package p\n\n// Sum adds the values.\n// Empty input returns zero.\nfunc Sum(xs []int) int {\n\treturn 0\n}\n",
        Some("Sum adds the values.\nEmpty input returns zero.")
    )]
    #[case::block_comment(
        "package p\n\n/* Sum adds the values. */\nfunc Sum(xs []int) int {\n\treturn 0\n}\n",
        Some("Sum adds the values.")
    )]
    #[case::blank_line_detaches(
        "package p\n\n// Stray comment.\n\nfunc Sum(xs []int) int {\n\treturn 0\n}\n",
        None
    )]
    #[case::no_comment("package p\n\nfunc Sum(xs []int) int {\n\treturn 0\n}\n", None)]
    fn extracts_go_doc_comment(#[case] src: &str, #[case] expected: Option<&str>) {
        let funcs = parse_functions(src);
        assert_eq!(funcs[0].doc.as_deref(), expected);
    }

    #[test]
    fn extracts_doc_comment_on_method_declaration() {
        let src = "package p\n\ntype S struct{}\n\n// Get returns the value.\nfunc (s *S) Get() int {\n\treturn 1\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs[0].doc.as_deref(), Some("Get returns the value."));
    }

    fn parse_tree(src: &str) -> TreeNode {
        let mut parser = GoParser::new();
        parser.parse(src).unwrap()
    }

    fn find_label<'a>(node: &'a TreeNode, label: &str) -> Option<&'a TreeNode> {
        if node.label == label {
            return Some(node);
        }
        node.children.iter().find_map(|c| find_label(c, label))
    }

    #[test]
    fn extracts_top_level_function_name_and_lines() {
        let src = "package p\n\nfunc First() int {\n    return 1\n}\n\nfunc Second() int {\n    x := 1\n    return x\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert_eq!(funcs[0].name, "First");
        assert_eq!(funcs[1].name, "Second");
        assert_eq!(funcs[0].start_line, 3);
        assert_eq!(funcs[0].end_line, 5);
        assert_eq!(funcs[1].start_line, 7);
        assert_eq!(funcs[1].end_line, 10);
    }

    #[test]
    fn language_identifier_is_go() {
        let parser = GoParser::new();
        assert_eq!(parser.language(), "go");
    }

    #[rstest]
    #[case::pointer_receiver(
        "package p\nfunc (s *Service) Compute(x int) int { return x }\n",
        "Service::Compute"
    )]
    #[case::value_receiver(
        "package p\nfunc (s Service) Compute(x int) int { return x }\n",
        "Service::Compute"
    )]
    #[case::unnamed_receiver(
        "package p\nfunc (Service) Compute() int { return 0 }\n",
        "Service::Compute"
    )]
    #[case::generic_receiver(
        "package p\nfunc (s *Service[T]) Compute() int { return 0 }\n",
        "Service::Compute"
    )]
    fn methods_are_qualified_by_receiver(#[case] src: &str, #[case] expected: &str) {
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1, "expected one method in: {src}");
        assert_eq!(funcs[0].name, expected);
    }

    #[test]
    fn closures_inside_functions_do_not_surface_as_separate_units() {
        // Function bodies are atomic: closures bound to a `:=` only
        // contribute to the parent's tree, mirroring how `lens-rust`
        // keeps closures inside their parent fn and `lens-ts` skips
        // inner functions.
        let src = "package p\nfunc outer() func() int {\n    inner := func() int { return 1 }\n    return inner\n}\n";
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["outer"]);
    }

    #[test]
    fn interface_method_signatures_are_not_extracted() {
        // Interface declarations carry method element shapes
        // (`method_elem`) but no bodies, so they aren't analysable
        // function units. `extract_functions` must skip them entirely.
        let src = "package p\ntype Foo interface {\n    Bar() int\n    Baz(x int) string\n}\n";
        let funcs = parse_functions(src);
        assert!(
            funcs.is_empty(),
            "interface methods must not surface as functions, got {:?}",
            funcs.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        );
    }

    fn allocation_source(expr: &str) -> String {
        format!("package p\nfunc mk(s string) string {{ return s }}\nfunc f() {{ _ = {expr} }}\n")
    }

    /// The `new` / `make` argument shapes the grammar has always covered:
    /// anything its *type* production reaches, plus the identifier-shaped
    /// spellings that coincide with one. These must keep parsing whatever
    /// happens to the Go 1.27 shapes pinned below.
    #[rstest]
    #[case::type_argument("new(string)")]
    #[case::identifier("new(x)")]
    #[case::identifier_shaped_literal("new(false)")]
    #[case::composite_type("new(map[string]int)")]
    #[case::qualified_type("new(pkg.T)")]
    #[case::make_slice("make([]int, 5)")]
    #[case::make_map_with_cap("make(map[string]int, 8)")]
    fn type_shaped_allocation_arguments_parse(#[case] expr: &str) {
        let src = allocation_source(expr);
        let mut parser = GoParser::new();
        parser
            .parse(&src)
            .unwrap_or_else(|err| panic!("{expr} should parse: {err}"));
        let funcs = parser.extract_functions(&src).unwrap();
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["mk", "f"]);
    }

    /// Go 1.27 generalized `new` to accept an arbitrary expression, not
    /// just a type ([spec: Allocation](https://go.dev/ref/spec#Allocation)),
    /// and `tree-sitter-go` still restricts that slot to its type
    /// production — so these shapes do not parse (issue #494). We wait for
    /// upstream rather than carrying a patched grammar; the cost is bounded
    /// because a walked file that fails to parse is warned about and
    /// skipped instead of failing the run.
    ///
    /// The limitation is pinned deliberately: a `tree-sitter-go` bump that
    /// fixes it breaks this test, which is the signal to delete it and move
    /// these cases up into [`type_shaped_allocation_arguments_parse`].
    #[rstest]
    #[case::numeric_literal("new(3)")]
    #[case::unary_expression("new(-1)")]
    #[case::call_expression("new(mk(\"hi\"))")]
    #[case::conversion("new(int64(2))")]
    #[case::string_literal("new(\"16Gi\")")]
    fn go_1_27_new_expr_arguments_do_not_parse_yet(#[case] expr: &str) {
        let err = GoParser::new()
            .parse(&allocation_source(expr))
            .expect_err("upstream tree-sitter-go fixed new(expr): see the doc comment");
        assert!(format!("{err}").contains("parse"));
    }

    #[test]
    fn parse_returns_error_for_invalid_go() {
        // `func !!! {` has no recognisable `func name(...)` shape — the
        // parser builds a tree with `ERROR` nodes and we surface that
        // as `GoParseError::Syntax` rather than handing back a partial
        // tree.
        let mut parser = GoParser::new();
        let err = parser.parse("package p\nfunc !!! {").unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }

    /// Identifier-bearing leaves must round-trip their source text into
    /// the lowered node's `value` so value-aware comparisons can tell
    /// `x` from `y`, `Foo.bar` from `Foo.baz`, and `package p` from
    /// `package q`. Without this, every identifier collapses to an
    /// empty value and clones-with-rename score 1.0 against unrelated
    /// functions. The five kinds share a single match arm in
    /// `node_value`; this rstest pins that arm so deleting it
    /// regresses every covered shape.
    #[rstest]
    #[case::plain_identifier(
        "package p\nfunc f() int { x := 1; return x }\n",
        "identifier",
        &["x"][..],
    )]
    #[case::type_identifier(
        "package p\nfunc f(s Service) Service { return s }\n",
        "type_identifier",
        &["Service"][..],
    )]
    #[case::field_identifier(
        "package p\ntype S struct{}\nfunc f(s S) int { return s.foo }\n",
        "field_identifier",
        &["foo"][..],
    )]
    #[case::package_identifier(
        "package mypkg\nfunc f() int { return 0 }\n",
        "package_identifier",
        &["mypkg"][..],
    )]
    #[case::label_name(
        "package p\nfunc f() {\n    Outer:\n    for {\n        break Outer\n    }\n}\n",
        "label_name",
        &["Outer"][..],
    )]
    fn identifier_leaves_carry_their_source_text_as_value(
        #[case] src: &str,
        #[case] label: &str,
        #[case] expected_values: &[&str],
    ) {
        fn collect_values<'a>(node: &'a TreeNode, label: &str, out: &mut Vec<&'a str>) {
            if node.label == label {
                out.push(node.value.as_str());
            }
            for c in &node.children {
                collect_values(c, label, out);
            }
        }
        let tree = parse_tree(src);
        let mut got = Vec::new();
        collect_values(&tree, label, &mut got);
        for want in expected_values {
            assert!(
                got.contains(want),
                "{label} nodes should carry their text as `value` (looking for {want:?}); got {got:?}",
            );
        }
    }

    #[test]
    fn parse_records_function_declaration_label_and_name_value() {
        let tree = parse_tree("package p\nfunc Hello() int { return 1 }\n");
        let func = find_label(&tree, "function_declaration").expect("function_declaration present");
        assert_eq!(
            func.value, "Hello",
            "function_declaration should expose its name as the node value",
        );
    }

    #[test]
    fn parse_records_method_declaration_with_method_name_value() {
        let tree = parse_tree("package p\nfunc (s *S) Compute() int { return 0 }\n");
        let method = find_label(&tree, "method_declaration").expect("method_declaration present");
        assert_eq!(method.value, "Compute");
    }

    #[test]
    fn parse_walks_into_expressions_so_call_nodes_appear() {
        let tree = parse_tree("package p\nfunc f() { g(1) }\n");
        assert!(
            find_label(&tree, "call_expression").is_some(),
            "call_expression should be present in the tree",
        );
    }

    #[test]
    fn parse_distinguishes_for_if_and_switch_labels() {
        let src = r#"
package p
func f() {
    for i := 0; i < 1; i++ {
    }
    if true {
    }
    switch x {
    case 1:
    }
}
"#;
        let tree = parse_tree(src);
        assert!(
            find_label(&tree, "for_statement").is_some(),
            "for_statement label missing",
        );
        assert!(
            find_label(&tree, "if_statement").is_some(),
            "if_statement label missing",
        );
        assert!(
            find_label(&tree, "expression_switch_statement").is_some(),
            "expression_switch_statement label missing",
        );
    }

    #[test]
    fn clones_are_detected_as_highly_similar() {
        let src = r#"
package p
func original(xs []int) int {
    total := 0
    for _, x := range xs {
        total += x
    }
    return total
}

func cloned(ys []int) int {
    sum := 0
    for _, y := range ys {
        sum += y
    }
    return sum
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim > 0.9,
            "expected renamed clone to stay > 0.9 similar, got {sim}",
        );
    }

    #[test]
    fn structurally_different_functions_score_low() {
        let src = r#"
package p
func loopy(xs []int) int {
    total := 0
    for _, x := range xs {
        total += x
    }
    return total
}

func recursive(n int) int {
    if n == 0 {
        return 0
    }
    return n + recursive(n-1)
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim < 0.8,
            "expected structurally different functions to score < 0.8, got {sim}",
        );
    }

    #[test]
    fn find_similar_functions_reports_clone_pair() {
        let src = r#"
package p
func a(xs []int) int {
    t := 0
    for _, x := range xs {
        t += x
    }
    return t
}

func b(ys []int) int {
    s := 0
    for _, y := range ys {
        s += y
    }
    return s
}

func c(n int) int {
    if n == 0 {
        return 0
    }
    return n * 2
}
"#;
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        let names = (pairs[0].a.name.as_str(), pairs[0].b.name.as_str());
        assert!(names == ("a", "b") || names == ("b", "a"), "got {names:?}");
    }

    /// Default `extract_functions` keeps every item — even what
    /// `--exclude-tests` would drop. If the boolean guards in
    /// `extract_with` ever degrade to constants the default contract
    /// would silently break.
    #[rstest]
    #[case::test_function(
        "package p\nfunc TestSomething(t *testing.T) {\n    _ = 1\n}\n",
        &["TestSomething"][..],
    )]
    #[case::benchmark_function(
        "package p\nfunc BenchmarkAdd(b *testing.B) {\n    _ = 1\n}\n",
        &["BenchmarkAdd"][..],
    )]
    #[case::example_function(
        "package p\nfunc ExampleHello() {\n    _ = 1\n}\n",
        &["ExampleHello"][..],
    )]
    #[case::fuzz_function(
        "package p\nfunc FuzzParser(f *testing.F) {\n    _ = 1\n}\n",
        &["FuzzParser"][..],
    )]
    fn default_extraction_includes_test_flavoured_items(
        #[case] src: &str,
        #[case] expected: &[&str],
    ) {
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names, expected,
            "default extraction must keep every item; only --exclude-tests should drop them",
        );
    }

    #[test]
    fn extraction_marks_go_test_scaffolding() {
        // Production code surrounded by every shape `go test` would
        // discover: a `Test*` function, a `Benchmark*`, an `Example*`,
        // and a `Fuzz*`. Methods are not marked by Go's test-name
        // convention because discovery only applies to free functions.
        let src = r#"
package p

import "testing"

func production(x int) int {
    return x + 1
}

func TestUnit(t *testing.T) {
    if production(0) != 1 {
        t.Fatal("bad")
    }
}

func BenchmarkAdd(b *testing.B) {
    for i := 0; i < b.N; i++ {
        production(i)
    }
}

func ExampleProduction() {
    _ = production(0)
}

func FuzzProduction(f *testing.F) {
    f.Add(0)
}

type Service struct{}

func (s *Service) Compute(x int) int {
    return production(x)
}
"#;
        let mut parser = GoParser::new();
        let funcs = parser.extract_functions(src).unwrap();
        let flags: Vec<_> = funcs.iter().map(|f| (f.name.as_str(), f.is_test)).collect();
        assert_eq!(
            flags,
            [
                ("production", false),
                ("TestUnit", true),
                ("BenchmarkAdd", true),
                ("ExampleProduction", true),
                ("FuzzProduction", true),
                ("Service::Compute", false),
            ]
        );
    }

    #[test]
    fn extraction_marks_functions_without_test_markers_as_production() {
        let src = "package p\nfunc a() int { return 0 }\nfunc b() int { return 1 }\n";
        let funcs = parse_functions(src);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[test]
    fn extraction_surfaces_parse_errors() {
        let mut parser = GoParser::new();
        let err = parser
            .extract_functions("package p\nfunc !!! {")
            .unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }

    /// Go has no attribute syntax: a directive is a comment the
    /// toolchain reads, and the spelling is exact. The ones that
    /// publish a symbol to a caller no Go call site names — cgo's
    /// `//export`, `//go:linkname` — have to be told apart from prose
    /// that merely starts with the same word.
    #[rstest]
    #[case::codegen_directive("//go:noinline\nfunc f() {}\n", vec!["go:noinline"])]
    #[case::directive_with_arguments(
        "//go:linkname f runtime.f\nfunc f() {}\n",
        vec!["go:linkname"],
    )]
    #[case::cgo_export("//export F\nfunc f() {}\n", vec!["export"])]
    #[case::several("//go:generate stringer\n//go:noinline\nfunc f() {}\n", vec!["go:generate", "go:noinline"])]
    #[case::prose_is_not_a_directive("// export the widget\nfunc f() {}\n", Vec::new())]
    #[case::spaced_is_not_a_directive("// go:noinline\nfunc f() {}\n", Vec::new())]
    #[case::export_needs_a_name("//export\nfunc f() {}\n", Vec::new())]
    #[case::no_comment("func f() {}\n", Vec::new())]
    fn directive_names_reads_only_real_directives(#[case] body: &str, #[case] expected: Vec<&str>) {
        let source = format!("package app\n\n{body}");
        let bytes = source.as_bytes();
        let tree = super::parse_tree(&source).unwrap();
        let mut found = None;
        walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
            found = Some(directive_names(site.node, bytes));
        });
        assert_eq!(found.unwrap_or_default(), expected);
    }
}

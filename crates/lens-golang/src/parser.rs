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

use lens_domain::{FunctionDef, LanguageParser, TreeNode, qualify as qualify_name};
use tree_sitter::{Node, Parser};

use crate::attrs::name_looks_like_test_function;

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
    type Error = GoParseError;

    fn language(&self) -> &'static str {
        "go"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, Self::Error> {
        let tree = parse_tree(source)?;
        let root = tree.root_node();
        let bytes = source.as_bytes();
        Ok(build_tree(root, bytes, /* is_root = */ true))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, Self::Error> {
        extract_with(source, ExtractOptions::default())
    }
}

/// Like [`GoParser::extract_functions`] but drops items that look like
/// `go test` scaffolding: free functions whose names start with
/// `Test`, `Benchmark`, `Example`, or `Fuzz` (followed by `_`,
/// upper-case, or end-of-name).
///
/// Methods are kept regardless — `(*S).TestSomething` is unusual but
/// not test scaffolding, and the path-level filter at the analyzer
/// layer already excludes `*_test.go` files for the common case.
pub fn extract_functions_excluding_tests(source: &str) -> Result<Vec<FunctionDef>, GoParseError> {
    extract_with(
        source,
        ExtractOptions {
            exclude_tests: true,
        },
    )
}

#[derive(Default, Clone, Copy)]
struct ExtractOptions {
    exclude_tests: bool,
}

fn extract_with(source: &str, opts: ExtractOptions) -> Result<Vec<FunctionDef>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let mut out = Vec::new();
    let mut cursor = tree.root_node().walk();
    for child in tree.root_node().named_children(&mut cursor) {
        match child.kind() {
            "function_declaration" => {
                if let Some(def) = function_def_from(child, bytes, None, opts) {
                    out.push(def);
                }
            }
            "method_declaration" => {
                let owner = method_receiver_type(child, bytes);
                // Methods can't be filtered by Go's test-name convention:
                // `Test*` discovery only applies to free functions. Pass
                // a synthetic `ExtractOptions` so the name filter is a
                // no-op for receivers — a method called `TestX` on a
                // production type is still production code.
                let method_opts = ExtractOptions {
                    exclude_tests: false,
                };
                if let Some(def) = function_def_from(child, bytes, owner.as_deref(), method_opts) {
                    out.push(def);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

fn parse_tree(source: &str) -> Result<tree_sitter::Tree, GoParseError> {
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

/// Lower a `function_declaration` or `method_declaration` node into a
/// [`FunctionDef`]. Returns `None` when the declaration has no body
/// (forward declarations, syntax errors below the root) or when the
/// `exclude_tests` filter rejects the name.
fn function_def_from(
    node: Node<'_>,
    source: &[u8],
    owner: Option<&str>,
    opts: ExtractOptions,
) -> Option<FunctionDef> {
    let body = node.child_by_field_name("body")?;
    let raw_name = function_name_text(node, source)?;
    if opts.exclude_tests && owner.is_none() && name_looks_like_test_function(raw_name) {
        return None;
    }
    let qualified = qualify_name(owner, raw_name);
    let start_line = node.start_position().row + 1;
    let end_line = node.end_position().row + 1;
    let tree = build_tree(body, source, /* is_root = */ true);
    Some(FunctionDef {
        name: qualified,
        start_line,
        end_line,
        tree,
    })
}

/// Resolve the user-visible name of a `function_declaration` or
/// `method_declaration`. Free functions use the `name: identifier`
/// field; methods use `name: field_identifier`.
fn function_name_text<'a>(node: Node<'_>, source: &'a [u8]) -> Option<&'a str> {
    node.child_by_field_name("name")
        .and_then(|n| n.utf8_text(source).ok())
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
fn method_receiver_type(node: Node<'_>, source: &[u8]) -> Option<String> {
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
        "type_identifier" => node.utf8_text(source).ok().map(str::to_owned),
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

fn node_value(node: Node<'_>, source: &[u8]) -> String {
    match node.kind() {
        "identifier" | "field_identifier" | "package_identifier" | "type_identifier"
        | "label_name" => node
            .utf8_text(source)
            .map(str::to_owned)
            .unwrap_or_default(),
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

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = GoParser::new();
        parser.extract_functions(src).unwrap()
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
    fn excluding_tests_drops_go_test_scaffolding() {
        // Production code surrounded by every shape `go test` would
        // discover: a `Test*` function, a `Benchmark*`, an `Example*`,
        // and a `Fuzz*`. The bare production function and the method
        // (which test discovery never reaches) must both survive.
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
        let funcs = extract_functions_excluding_tests(src).unwrap();
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["production", "Service::Compute"]);
    }

    #[test]
    fn excluding_tests_keeps_default_extraction_with_no_test_markers() {
        // No test-shaped items — the filter should be a no-op so the
        // public surface still reports every production function.
        let src = "package p\nfunc a() int { return 0 }\nfunc b() int { return 1 }\n";
        let baseline = parse_functions(src);
        let filtered = extract_functions_excluding_tests(src).unwrap();
        assert_eq!(
            baseline.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            filtered.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
        );
    }

    #[test]
    fn excluding_tests_surfaces_parse_errors() {
        let err = extract_functions_excluding_tests("package p\nfunc !!! {").unwrap_err();
        assert!(format!("{err}").contains("parse"));
    }
}

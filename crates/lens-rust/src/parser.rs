//! syn-based implementation of [`lens_domain::LanguageParser`] for Rust.

use lens_domain::{
    FunctionDef, LanguageParseError, LanguageParser, TreeNode, qualify as qualify_name,
};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;

use crate::common::{WalkOptions, walk_fn_items};

/// A Rust-language parser backed by [`syn`].
///
/// Stateless; all work happens inside [`LanguageParser::parse`] and
/// [`LanguageParser::extract_functions`]. The struct exists so that
/// callers can swap in a tree-sitter backend later without changing
/// downstream code.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustParser;

impl RustParser {
    pub fn new() -> Self {
        Self
    }
}

/// A Rust function annotated with its lexical module context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustFunctionDef {
    /// The language-agnostic function payload used by existing analyzers.
    pub function: FunctionDef,
    /// Bare function or method name as written in the signature.
    pub name: String,
    /// Absolute lexical name rooted at `crate`, e.g. `crate::a::parse`
    /// or `crate::a::Service::handle`.
    pub qualified_name: String,
    /// Absolute lexical module path rooted at `crate`.
    pub module: String,
    /// `impl` self-type or trait name for methods, `None` for free functions.
    pub impl_owner: Option<String>,
}

/// Parse failures surfaced by [`RustParser`].
#[derive(Debug, thiserror::Error)]
pub enum RustParseError {
    #[error("failed to parse Rust source: {0}")]
    Syn(#[from] syn::Error),
}

/// Extract functions while preserving the caller-provided file module as
/// the base for inline modules.
pub fn extract_functions_with_modules(
    source: &str,
    base_module: &str,
) -> Result<Vec<RustFunctionDef>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    extract_module_functions(&file.items, base_module, false, &mut out);
    Ok(out)
}

impl LanguageParser for RustParser {
    fn language(&self) -> &'static str {
        "rust"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, LanguageParseError> {
        let file = syn::parse_file(source)
            .map_err(RustParseError::from)
            .map_err(|err| LanguageParseError::new(self.language(), err))?;
        Ok(token_stream_to_tree("File", file.to_token_stream()))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, LanguageParseError> {
        extract_with(source, WalkOptions::default())
            .map_err(|err| LanguageParseError::new(self.language(), err))
    }
}

fn extract_with(source: &str, opts: WalkOptions) -> Result<Vec<FunctionDef>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut out = Vec::new();
    walk_fn_items(&file.items, opts, &mut |site| {
        out.push(FunctionDef {
            name: qualify_name(site.owner, &site.sig.ident.to_string()),
            start_line: site.sig.span().start().line,
            end_line: site.block.span().end().line,
            is_test: site.is_test,
            tree: token_stream_to_tree("Block", site.block.to_token_stream()),
        });
    });
    Ok(out)
}

fn extract_module_functions(
    items: &[syn::Item],
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    for item in items {
        extract_item_functions(item, module, in_test_context, out);
    }
}

fn extract_item_functions(
    item: &syn::Item,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    match item {
        syn::Item::Fn(item_fn) => {
            let name = item_fn.sig.ident.to_string();
            let function = FunctionDef {
                name: name.clone(),
                start_line: item_fn.sig.span().start().line,
                end_line: item_fn.block.span().end().line,
                is_test: in_test_context || crate::attrs::is_test_function(&item_fn.attrs),
                tree: token_stream_to_tree("Block", item_fn.block.to_token_stream()),
            };
            out.push(RustFunctionDef {
                function,
                qualified_name: qualify_module(module, &name),
                module: module.to_owned(),
                impl_owner: None,
                name,
            });
        }
        syn::Item::Impl(item_impl) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_impl.attrs);
            extract_impl_functions(item_impl, module, in_test_context || item_is_test, out);
        }
        syn::Item::Trait(item_trait) => {
            let item_is_test = crate::attrs::has_cfg_test(&item_trait.attrs);
            extract_trait_functions(item_trait, module, in_test_context || item_is_test, out);
        }
        syn::Item::Mod(item_mod) => {
            if let Some((_, nested_items)) = &item_mod.content {
                let nested_module = qualify_module(module, &item_mod.ident.to_string());
                let item_is_test = crate::attrs::has_cfg_test(&item_mod.attrs);
                extract_module_functions(
                    nested_items,
                    &nested_module,
                    in_test_context || item_is_test,
                    out,
                );
            }
        }
        _ => {}
    }
}

fn extract_impl_functions(
    item_impl: &syn::ItemImpl,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = crate::common::type_path_last_ident(&item_impl.self_ty);
    for impl_item in &item_impl.items {
        let syn::ImplItem::Fn(method) = impl_item else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function_name = qualify_name(owner.as_deref(), &name);
        let qualified_name = owner.as_ref().map_or_else(
            || qualify_module(module, &name),
            |owner| qualify_module(module, &format!("{owner}::{name}")),
        );
        let function = FunctionDef {
            name: function_name,
            start_line: method.sig.span().start().line,
            end_line: method.block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            tree: token_stream_to_tree("Block", method.block.to_token_stream()),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name,
            module: module.to_owned(),
            impl_owner: owner.clone(),
            name,
        });
    }
}

fn extract_trait_functions(
    item_trait: &syn::ItemTrait,
    module: &str,
    in_test_context: bool,
    out: &mut Vec<RustFunctionDef>,
) {
    let owner = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        let syn::TraitItem::Fn(method) = trait_item else {
            continue;
        };
        let Some(block) = &method.default else {
            continue;
        };
        let name = method.sig.ident.to_string();
        let function = FunctionDef {
            name: qualify_name(Some(&owner), &name),
            start_line: method.sig.span().start().line,
            end_line: block.span().end().line,
            is_test: in_test_context || crate::attrs::is_test_function(&method.attrs),
            tree: token_stream_to_tree("Block", block.to_token_stream()),
        };
        out.push(RustFunctionDef {
            function,
            qualified_name: qualify_module(module, &format!("{owner}::{name}")),
            module: module.to_owned(),
            impl_owner: Some(owner.clone()),
            name,
        });
    }
}

fn qualify_module(module: &str, name: &str) -> String {
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn token_stream_to_tree(label: &str, stream: TokenStream) -> TreeNode {
    let mut node = TreeNode::new(label, "");
    for tt in stream {
        node.push_child(token_tree_to_node(tt));
    }
    node
}

fn token_tree_to_node(tt: TokenTree) -> TreeNode {
    match tt {
        TokenTree::Group(group) => {
            let label = match group.delimiter() {
                Delimiter::Parenthesis => "Paren",
                Delimiter::Brace => "Brace",
                Delimiter::Bracket => "Bracket",
                Delimiter::None => "Group",
            };
            token_stream_to_tree(label, group.stream())
        }
        TokenTree::Ident(ident) => TreeNode::new("Ident", ident.to_string()),
        TokenTree::Punct(punct) => TreeNode::new("Punct", punct.as_char().to_string()),
        TokenTree::Literal(lit) => TreeNode::new("Lit", lit.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::{TSEDOptions, calculate_tsed, find_similar_functions};
    use rstest::rstest;

    fn parse_functions(src: &str) -> Vec<FunctionDef> {
        let mut parser = RustParser::new();
        parser.extract_functions(src).unwrap()
    }

    #[test]
    fn extracts_free_function_name_and_lines() {
        let src = "fn first() {}\nfn second() { let _x = 1; }\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 2);
        assert_eq!(funcs[0].name, "first");
        assert_eq!(funcs[1].name, "second");
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 1);
        assert_eq!(funcs[1].start_line, 2);
        assert_eq!(funcs[1].end_line, 2);
    }

    #[test]
    fn end_line_tracks_closing_brace_for_multi_line_function() {
        let src = "fn body() {\n    let x = 1;\n    let y = 2;\n}\n";
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].start_line, 1);
        assert_eq!(funcs[0].end_line, 4);
    }

    #[test]
    fn language_identifier_is_rust() {
        let parser = RustParser::new();
        assert_eq!(parser.language(), "rust");
    }

    #[test]
    fn parse_error_exposes_underlying_syn_error_via_source() {
        let mut parser = RustParser::new();
        let err = parser.parse("fn ??? {").unwrap_err();
        let source = std::error::Error::source(&err).expect("source should be Some");
        // The underlying syn error should round-trip through Display so the
        // chained error message stays intact.
        assert!(!format!("{source}").is_empty());
    }

    #[test]
    fn extracts_impl_methods_with_qualified_names() {
        let src = r#"
struct Foo;
impl Foo {
    fn bar(&self) -> i32 { 1 }
    fn baz(&self) -> i32 { 2 }
}
"#;
        let funcs = parse_functions(src);
        let names: Vec<_> = funcs.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["Foo::bar", "Foo::baz"]);
    }

    #[test]
    fn extracts_trait_default_methods_only() {
        let src = r#"
trait T {
    fn required(&self);
    fn with_default(&self) -> u32 { 42 }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "T::with_default");
    }

    #[test]
    fn extracts_functions_inside_inline_modules() {
        let src = r#"
mod inner {
    fn hidden() -> u32 { 0 }
}
"#;
        let funcs = parse_functions(src);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs[0].name, "hidden");
    }

    #[test]
    fn module_aware_extraction_preserves_lexical_qualified_names() {
        let src = r#"
mod inner {
    fn parse() {}
    struct S;
    impl S {
        fn call(&self) {}
    }
}
"#;
        let funcs = extract_functions_with_modules(src, "crate::outer").unwrap();
        let names: Vec<_> = funcs
            .iter()
            .map(|f| {
                (
                    f.name.as_str(),
                    f.qualified_name.as_str(),
                    f.module.as_str(),
                    f.impl_owner.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            names,
            [
                (
                    "parse",
                    "crate::outer::inner::parse",
                    "crate::outer::inner",
                    None
                ),
                (
                    "call",
                    "crate::outer::inner::S::call",
                    "crate::outer::inner",
                    Some("S"),
                ),
            ]
        );
    }

    #[test]
    fn module_aware_extraction_propagates_test_contexts() {
        let src = r#"
#[test]
fn direct_test() {}

#[cfg(test)]
mod tests {
    fn helper() {}
    struct Bag;
    impl Bag {
        fn fixture() {}
    }
    trait Harness {
        fn default_helper() {}
    }
}
"#;
        let funcs = extract_functions_with_modules(src, "crate").unwrap();
        let flags: Vec<_> = funcs
            .iter()
            .map(|f| (f.qualified_name.as_str(), f.function.is_test))
            .collect();
        assert_eq!(
            flags,
            [
                ("crate::direct_test", true),
                ("crate::tests::helper", true),
                ("crate::tests::Bag::fixture", true),
                ("crate::tests::Harness::default_helper", true),
            ]
        );
    }

    #[test]
    fn parse_returns_error_for_invalid_rust() {
        let mut parser = RustParser::new();
        let err = parser.parse("fn ??? {").unwrap_err();
        assert!(format!("{err}").contains("failed to parse Rust source"));
    }

    #[test]
    fn clones_are_detected_as_highly_similar() {
        let src = r#"
fn original(xs: &[u32]) -> u32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}

fn cloned(ys: &[u32]) -> u32 {
    let mut sum = 0;
    for y in ys {
        sum += *y;
    }
    sum
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim > 0.9,
            "expected renamed clone to stay > 0.9 similar, got {sim}"
        );
    }

    #[test]
    fn structurally_different_functions_score_low() {
        let src = r#"
fn loopy(xs: &[u32]) -> u32 {
    let mut total = 0;
    for x in xs {
        total += *x;
    }
    total
}

fn recursive(n: u32) -> u32 {
    if n == 0 { 0 } else { n + recursive(n - 1) }
}
"#;
        let funcs = parse_functions(src);
        let opts = TSEDOptions::default();
        let sim = calculate_tsed(&funcs[0].tree, &funcs[1].tree, &opts);
        assert!(
            sim < 0.8,
            "expected structurally different functions to score < 0.8, got {sim}"
        );
    }

    /// Default `extract_functions` keeps every item — even the ones an
    /// `--exclude-tests` run would drop. Walking into a `#[cfg(test)]
    /// mod` / `impl` / `trait` and the test-tagged free fn inside each
    /// must still surface them; otherwise the boolean guards in
    /// `collect_*` could degrade to constants (mutant `&& → ||`) and
    /// silently break the default contract without any test catching it.
    #[rstest]
    #[case::cfg_test_mod(
        "#[cfg(test)]\nmod tests { fn helper() {} }\n",
        &["helper"][..],
    )]
    #[case::cfg_test_impl(
        "struct T;\n#[cfg(test)]\nimpl T { fn helper() {} }\n",
        &["T::helper"][..],
    )]
    #[case::cfg_test_trait(
        "#[cfg(test)]\ntrait Tr { fn def_method() -> u32 { 0 } }\n",
        &["Tr::def_method"][..],
    )]
    #[case::test_attr_free_fn("#[test]\nfn ut() {}\n", &["ut"][..])]
    #[case::test_attr_impl_method(
        "struct T;\nimpl T { #[test] fn fixture() {} }\n",
        &["T::fixture"][..],
    )]
    #[case::test_attr_trait_method(
        "trait Tr { #[test] fn default_test() -> u32 { 0 } }\n",
        &["Tr::default_test"][..],
    )]
    fn default_extraction_includes_test_attributed_items(
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
    fn extraction_marks_cfg_test_modules_and_test_attributed_fns() {
        // Production code surrounded by every shape the analyzer later
        // filters: a `#[test]` free fn, a `#[rstest]` fn, a `mod tests`
        // gated by `#[cfg(test)]`, and an `impl` block gated the same way.
        let src = r#"
fn production(x: i32) -> i32 { x + 1 }

#[test]
fn unit_test() { assert_eq!(production(0), 1); }

#[rstest]
fn parameterised_test() { assert_eq!(production(0), 1); }

#[cfg(test)]
mod tests {
    use super::*;
    fn helper() -> i32 { production(0) }
    fn other_helper() -> i32 { production(1) }
}

struct Bag;
#[cfg(test)]
impl Bag {
    fn fixture() -> Self { Self }
}
"#;
        let mut parser = RustParser::new();
        let funcs = parser.extract_functions(src).unwrap();
        let flags: Vec<_> = funcs.iter().map(|f| (f.name.as_str(), f.is_test)).collect();
        assert_eq!(
            flags,
            [
                ("production", false),
                ("unit_test", true),
                ("parameterised_test", true),
                ("helper", true),
                ("other_helper", true),
                ("Bag::fixture", true),
            ]
        );
    }

    #[test]
    fn extraction_marks_functions_without_test_attrs_as_production() {
        let src = "fn a() {}\nfn b() {}\n";
        let funcs = parse_functions(src);
        assert!(funcs.iter().all(|f| !f.is_test));
    }

    #[test]
    fn find_similar_functions_reports_clone_pair() {
        let src = r#"
fn a(xs: &[u32]) -> u32 {
    let mut t = 0;
    for x in xs { t += *x; }
    t
}

fn b(ys: &[u32]) -> u32 {
    let mut s = 0;
    for y in ys { s += *y; }
    s
}

fn c(n: u32) -> u32 {
    if n == 0 { 0 } else { n * 2 }
}
"#;
        let funcs = parse_functions(src);
        let pairs = find_similar_functions(&funcs, 0.85, &TSEDOptions::default());
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].a.name, "a");
        assert_eq!(pairs[0].b.name, "b");
    }
}

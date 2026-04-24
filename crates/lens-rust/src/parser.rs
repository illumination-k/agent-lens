//! syn-based implementation of [`lens_domain::LanguageParser`] for Rust.

use lens_domain::{FunctionDef, LanguageParser, TreeNode};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::{ImplItem, Item, ItemFn, ItemImpl, ItemTrait, TraitItem};

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

/// Parse failures surfaced by [`RustParser`].
#[derive(Debug)]
pub enum RustParseError {
    Syn(syn::Error),
}

impl std::fmt::Display for RustParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Syn(e) => write!(f, "failed to parse Rust source: {e}"),
        }
    }
}

impl std::error::Error for RustParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Syn(e) => Some(e),
        }
    }
}

impl From<syn::Error> for RustParseError {
    fn from(value: syn::Error) -> Self {
        Self::Syn(value)
    }
}

impl LanguageParser for RustParser {
    type Error = RustParseError;

    fn language(&self) -> &'static str {
        "rust"
    }

    fn parse(&mut self, source: &str) -> Result<TreeNode, Self::Error> {
        let file = syn::parse_file(source)?;
        Ok(token_stream_to_tree("File", file.to_token_stream()))
    }

    fn extract_functions(&mut self, source: &str) -> Result<Vec<FunctionDef>, Self::Error> {
        let file = syn::parse_file(source)?;
        let mut out = Vec::new();
        for item in &file.items {
            collect_item(item, &mut out);
        }
        Ok(out)
    }
}

fn collect_item(item: &Item, out: &mut Vec<FunctionDef>) {
    match item {
        Item::Fn(item_fn) => out.push(function_def_from_fn(item_fn)),
        Item::Impl(item_impl) => collect_impl(item_impl, out),
        Item::Trait(item_trait) => collect_trait(item_trait, out),
        Item::Mod(item_mod) => {
            if let Some((_, items)) = &item_mod.content {
                for nested in items {
                    collect_item(nested, out);
                }
            }
        }
        _ => {}
    }
}

fn collect_impl(item_impl: &ItemImpl, out: &mut Vec<FunctionDef>) {
    let self_name = type_path_last_ident(&item_impl.self_ty);
    for impl_item in &item_impl.items {
        if let ImplItem::Fn(method) = impl_item {
            let qualified = qualify_name(self_name.as_deref(), &method.sig.ident.to_string());
            out.push(FunctionDef {
                name: qualified,
                start_line: line_of(&method.sig),
                end_line: line_of_end(&method.block),
                tree: token_stream_to_tree("Block", method.block.to_token_stream()),
            });
        }
    }
}

fn collect_trait(item_trait: &ItemTrait, out: &mut Vec<FunctionDef>) {
    let trait_name = item_trait.ident.to_string();
    for trait_item in &item_trait.items {
        if let TraitItem::Fn(method) = trait_item {
            let Some(block) = &method.default else {
                continue;
            };
            let qualified = qualify_name(Some(&trait_name), &method.sig.ident.to_string());
            out.push(FunctionDef {
                name: qualified,
                start_line: line_of(&method.sig),
                end_line: line_of_end(block),
                tree: token_stream_to_tree("Block", block.to_token_stream()),
            });
        }
    }
}

fn function_def_from_fn(item_fn: &ItemFn) -> FunctionDef {
    FunctionDef {
        name: item_fn.sig.ident.to_string(),
        start_line: line_of(&item_fn.sig),
        end_line: line_of_end(&item_fn.block),
        tree: token_stream_to_tree("Block", item_fn.block.to_token_stream()),
    }
}

fn qualify_name(owner: Option<&str>, method: &str) -> String {
    match owner {
        Some(owner) => format!("{owner}::{method}"),
        None => method.to_owned(),
    }
}

fn type_path_last_ident(ty: &syn::Type) -> Option<String> {
    if let syn::Type::Path(type_path) = ty {
        type_path
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
    } else {
        None
    }
}

fn line_of<T: Spanned>(item: &T) -> usize {
    item.span().start().line
}

fn line_of_end<T: Spanned>(item: &T) -> usize {
    item.span().end().line
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
        assert_eq!(funcs[1].start_line, 2);
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

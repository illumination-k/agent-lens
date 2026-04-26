//! Collect `pub` declarations from a Rust module tree.
//!
//! Walks the [`CrateModule`] list produced by [`build_module_tree`] and
//! returns one [`PublicItem`] per top-level public declaration:
//! `pub fn`, `pub struct`, `pub enum`, `pub trait`, `pub type`,
//! `pub const`, `pub static`, `pub union`, and `#[macro_export]`
//! macros.
//!
//! Scope, deliberately narrow:
//!
//! * Only the canonical `pub` visibility (`syn::Visibility::Public`) is
//!   collected. `pub(crate)`, `pub(super)`, and `pub(in path)` carry
//!   their own intentional restriction and would muddy the dead-API
//!   signal.
//! * Items inside an `impl` block (inherent methods, associated
//!   functions / consts / types) and inside a `trait` body are *not*
//!   collected separately. Their liveness rides on the enclosing
//!   `struct` / `enum` / `trait`, which we *do* collect — tracking
//!   methods individually would require receiver-aware path resolution
//!   that the coupling extractor doesn't do today.
//! * Items inside `#[cfg(test)]` modules are skipped. They cannot form
//!   part of the production API and would otherwise look "dead" to
//!   every non-test consumer.
//! * `pub mod` declarations are not collected as items in their own
//!   right. [`build_module_tree`] strips `Item::Mod` from each
//!   parent's item list while flattening the tree, so a `pub mod foo;`
//!   never reaches this walker. Liveness of a module is reflected in
//!   the liveness of the items it contains.
//!
//! [`build_module_tree`]: crate::build_module_tree

use lens_domain::{PublicItem, PublicItemKind};
use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::{Attribute, Item, Visibility};

use crate::attrs::has_cfg_test;
use crate::coupling::CrateModule;

/// Collect every top-level `pub` declaration across `modules`.
///
/// Output order is deterministic: items are emitted in the order the
/// module tree is walked, then in source order within each module.
/// Modules whose declaration (or any ancestor's) carried
/// `#[cfg(test)]` are skipped wholesale — `build_module_tree`
/// propagates that gate via [`CrateModule::cfg_test`].
pub fn extract_public_items(modules: &[CrateModule]) -> Vec<PublicItem> {
    let mut out = Vec::new();
    for module in modules {
        if module.cfg_test {
            continue;
        }
        for item in &module.items {
            collect(item, module, &mut out);
        }
    }
    out
}

fn collect(item: &Item, module: &CrateModule, out: &mut Vec<PublicItem>) {
    match item {
        Item::Fn(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.sig.ident,
            PublicItemKind::Fn,
            it.span(),
            module,
            out,
        ),
        Item::Struct(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Struct,
            it.span(),
            module,
            out,
        ),
        Item::Enum(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Enum,
            it.span(),
            module,
            out,
        ),
        Item::Trait(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Trait,
            it.span(),
            module,
            out,
        ),
        Item::Type(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::TypeAlias,
            it.span(),
            module,
            out,
        ),
        Item::Const(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Const,
            it.span(),
            module,
            out,
        ),
        Item::Static(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Static,
            it.span(),
            module,
            out,
        ),
        Item::Union(it) => push_if_public(
            &it.vis,
            &it.attrs,
            &it.ident,
            PublicItemKind::Union,
            it.span(),
            module,
            out,
        ),
        Item::Macro(it) => collect_macro(it, module, out),
        // Other item kinds (Use, ForeignMod, Impl, ExternCrate, …)
        // either don't introduce public names of their own or contribute
        // through the items they contain. `Item::Mod` never reaches us
        // because `build_module_tree` lifted it out into a sibling
        // `CrateModule`.
        _ => {}
    }
}

fn collect_macro(item: &syn::ItemMacro, module: &CrateModule, out: &mut Vec<PublicItem>) {
    // Classic `macro_rules!` macros become public via
    // `#[macro_export]`; the standard `pub` visibility doesn't apply
    // to them. The unstable `pub macro` (macros 2.0) form is rare and
    // currently unsupported here.
    let exported = item.attrs.iter().any(|a| a.path().is_ident("macro_export"));
    if !exported {
        return;
    }
    let Some(ident) = item.ident.as_ref() else {
        return;
    };
    if has_cfg_test(&item.attrs) {
        return;
    }
    push(ident, PublicItemKind::Macro, item.span(), module, out);
}

fn push_if_public(
    vis: &Visibility,
    attrs: &[Attribute],
    ident: &syn::Ident,
    kind: PublicItemKind,
    span: Span,
    module: &CrateModule,
    out: &mut Vec<PublicItem>,
) {
    if !matches!(vis, Visibility::Public(_)) {
        return;
    }
    if has_cfg_test(attrs) {
        return;
    }
    push(ident, kind, span, module, out);
}

fn push(
    ident: &syn::Ident,
    kind: PublicItemKind,
    span: Span,
    module: &CrateModule,
    out: &mut Vec<PublicItem>,
) {
    out.push(PublicItem {
        module: module.path.clone(),
        name: ident.to_string(),
        kind,
        file: module.file.clone(),
        start_line: span.start().line,
        end_line: span.end().line,
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupling::build_module_tree;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn items_for(src: &str) -> Vec<PublicItem> {
        let dir = tempfile::tempdir().unwrap();
        let lib = write_file(dir.path(), "lib.rs", src);
        let modules = build_module_tree(&lib).unwrap();
        extract_public_items(&modules)
    }

    fn names(items: &[PublicItem]) -> Vec<&str> {
        items.iter().map(|i| i.name.as_str()).collect()
    }

    fn paths_and_names(items: &[PublicItem]) -> Vec<(&str, &str, PublicItemKind)> {
        items
            .iter()
            .map(|i| (i.module.as_str(), i.name.as_str(), i.kind))
            .collect()
    }

    #[test]
    fn collects_each_basic_pub_item_kind() {
        let src = r#"
            pub fn function_x() {}
            pub struct StructX;
            pub enum EnumX { A }
            pub trait TraitX {}
            pub type TypeX = u32;
            pub const CONST_X: u32 = 0;
            pub static STATIC_X: u32 = 0;
            pub union UnionX { a: u32 }
        "#;
        let items = items_for(src);
        let kinds: Vec<PublicItemKind> = items.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&PublicItemKind::Fn));
        assert!(kinds.contains(&PublicItemKind::Struct));
        assert!(kinds.contains(&PublicItemKind::Enum));
        assert!(kinds.contains(&PublicItemKind::Trait));
        assert!(kinds.contains(&PublicItemKind::TypeAlias));
        assert!(kinds.contains(&PublicItemKind::Const));
        assert!(kinds.contains(&PublicItemKind::Static));
        assert!(kinds.contains(&PublicItemKind::Union));
        assert_eq!(items.len(), 8);
    }

    #[test]
    fn skips_non_public_items() {
        let src = r#"
            fn private_fn() {}
            pub(crate) fn crate_only_fn() {}
            pub(super) fn super_only_fn() {}
            pub fn exposed_fn() {}
        "#;
        let items = items_for(src);
        assert_eq!(names(&items), vec!["exposed_fn"]);
    }

    #[test]
    fn collects_pub_items_inside_pub_modules_with_module_path() {
        let src = r#"
            pub mod inner {
                pub fn nested_fn() {}
                fn private_helper() {}
            }
        "#;
        let items = items_for(src);
        let mapped = paths_and_names(&items);
        assert_eq!(
            mapped,
            vec![("crate::inner", "nested_fn", PublicItemKind::Fn)]
        );
    }

    #[test]
    fn skips_items_under_cfg_test_modules() {
        let src = r#"
            pub fn alive() {}
            #[cfg(test)]
            pub mod tests {
                pub fn fixture() {}
            }
        "#;
        let items = items_for(src);
        let n = names(&items);
        assert_eq!(n, vec!["alive"]);
    }

    #[test]
    fn skips_individual_pub_items_attributed_with_cfg_test() {
        let src = r#"
            #[cfg(test)]
            pub fn test_only() {}
            pub fn always() {}
        "#;
        let items = items_for(src);
        assert_eq!(names(&items), vec!["always"]);
    }

    #[test]
    fn does_not_collect_methods_or_associated_items() {
        let src = r#"
            pub struct Holder;
            impl Holder {
                pub fn method(&self) {}
                pub const ASSOC: u32 = 0;
            }
            pub trait Tr {
                fn required(&self);
                const C: u32;
            }
        "#;
        let items = items_for(src);
        let n = names(&items);
        assert!(n.contains(&"Holder"));
        assert!(n.contains(&"Tr"));
        assert!(!n.contains(&"method"));
        assert!(!n.contains(&"ASSOC"));
        assert!(!n.contains(&"required"));
        assert!(!n.contains(&"C"));
    }

    #[test]
    fn collects_macro_export_macros_only() {
        let src = r#"
            #[macro_export]
            macro_rules! exported_macro {
                () => {};
            }
            macro_rules! local_macro {
                () => {};
            }
        "#;
        let items = items_for(src);
        let macros: Vec<&str> = items
            .iter()
            .filter(|i| i.kind == PublicItemKind::Macro)
            .map(|i| i.name.as_str())
            .collect();
        assert_eq!(macros, vec!["exported_macro"]);
    }

    #[test]
    fn line_ranges_track_source_position() {
        let src = "\npub fn first() {}\n\npub fn second() {\n    let _ = 1;\n}\n";
        let items = items_for(src);
        let first = items.iter().find(|i| i.name == "first").unwrap();
        let second = items.iter().find(|i| i.name == "second").unwrap();
        assert_eq!(first.start_line, 2);
        assert!(second.start_line < second.end_line);
    }

    #[test]
    fn carries_source_file_through_to_each_item() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lib.rs", "pub mod a;\n");
        let a_path = write_file(dir.path(), "a.rs", "pub fn helper() {}\n");
        let modules = build_module_tree(&dir.path().join("lib.rs")).unwrap();
        let items = extract_public_items(&modules);
        let helper = items.iter().find(|i| i.name == "helper").unwrap();
        assert_eq!(helper.file, a_path);
        assert_eq!(helper.module.as_str(), "crate::a");
    }
}

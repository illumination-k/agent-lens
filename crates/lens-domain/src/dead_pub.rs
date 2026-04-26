//! Language-agnostic dead `pub` item detection.
//!
//! A "dead pub" item is a publicly-exposed declaration (free function,
//! struct, enum, trait, type alias, const, static, union, …) that no
//! other module in the same crate references. Within a single crate the
//! signal is precise; for library crates the top-level `pub` items still
//! show up as dead because consumers live outside the crate, so callers
//! that care about that distinction can filter on
//! [`PublicItem::at_crate_root`].
//!
//! The matching works against the same [`CouplingEdge`] list that
//! `coupling` already builds:
//!
//! * `(_, item.module, item.name, _)` — direct reference (use, call,
//!   type, impl-for) of the item.
//! * `(_, item.module, "<item.name>::…", _)` — qualified reference
//!   (e.g. associated function `Foo::bar()` resolved to module
//!   `crate::a` with symbol `Foo::bar`).
//! * `(_, item.module, "*", _)` — glob `use` of the enclosing module;
//!   conservatively keeps every `pub` item in that module alive.
//!
//! Trait associated items and inherent methods are *not* collected as
//! standalone items here. They piggy-back on the visibility of the
//! enclosing trait or type: if the trait/type is dead, so is its API.
//!
//! Adapters (e.g. `lens-rust`) are responsible for producing the
//! [`PublicItem`] list; this module only knows how to fold edges over
//! it.

use std::path::PathBuf;

use crate::coupling::{CouplingEdge, ModulePath};

/// Categorisation of a `pub` declaration. Mirrors the small set of
/// `syn::Item` variants that are part of the public API surface; we
/// deliberately leave methods, associated consts, etc. to be tracked
/// via their enclosing item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PublicItemKind {
    Fn,
    Struct,
    Enum,
    Trait,
    TypeAlias,
    Const,
    Static,
    Union,
    Macro,
    Mod,
}

impl PublicItemKind {
    /// Snake-case name suitable for JSON serialisation and table cells.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fn => "fn",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::TypeAlias => "type",
            Self::Const => "const",
            Self::Static => "static",
            Self::Union => "union",
            Self::Macro => "macro",
            Self::Mod => "mod",
        }
    }
}

/// One `pub` declaration in the source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicItem {
    /// Module that lexically contains the item.
    pub module: ModulePath,
    /// Identifier as written (no qualification).
    pub name: String,
    pub kind: PublicItemKind,
    /// Source file the item was declared in (useful for jump-to in
    /// reports).
    pub file: PathBuf,
    /// 1-based inclusive line range of the item.
    pub start_line: usize,
    pub end_line: usize,
}

impl PublicItem {
    /// True iff the item lives directly in the crate root
    /// (`module == "crate"`). Library crates expose these to downstream
    /// consumers we cannot see, so callers may want to treat them
    /// differently from items nested in `pub` submodules.
    pub fn at_crate_root(&self) -> bool {
        self.module.as_str() == "crate"
    }

    /// True iff `edge` references this item.
    fn is_referenced_by(&self, edge: &CouplingEdge) -> bool {
        if edge.to != self.module {
            return false;
        }
        if edge.symbol == "*" {
            // Glob `use mod::*` — conservatively treat every pub item
            // in the target module as live. The alternative (counting
            // only items the consumer actually exercises) requires
            // type-aware analysis we don't do.
            return true;
        }
        if edge.symbol == self.name {
            return true;
        }
        // Qualified reference like `Foo::bar` keeps `Foo` alive as the
        // enclosing item even when `bar` itself isn't tracked
        // separately.
        let prefix = format!("{}::", self.name);
        edge.symbol.starts_with(&prefix)
    }
}

/// Find pub items that no edge references.
///
/// `items` should be the full list of `pub` declarations in the crate;
/// `edges` is the cross-module reference graph as produced by the
/// coupling extractor. Returns a stable list of unreferenced items
/// sorted by `(module, name)` so the report doesn't churn between runs.
///
/// Self-references (an item used only inside its own module) are *not*
/// counted as keeping the item alive — module-internal use doesn't make
/// a `pub` item part of the API contract.
pub fn find_dead_pub_items(items: Vec<PublicItem>, edges: &[CouplingEdge]) -> Vec<PublicItem> {
    let mut dead: Vec<PublicItem> = items
        .into_iter()
        .filter(|item| {
            !edges
                .iter()
                .any(|edge| edge.from != item.module && item.is_referenced_by(edge))
        })
        .collect();
    dead.sort_by(|a, b| a.module.cmp(&b.module).then_with(|| a.name.cmp(&b.name)));
    dead
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coupling::EdgeKind;

    fn item(module: &str, name: &str, kind: PublicItemKind) -> PublicItem {
        PublicItem {
            module: ModulePath::new(module),
            name: name.to_owned(),
            kind,
            file: PathBuf::from(format!("{module}.rs")),
            start_line: 1,
            end_line: 1,
        }
    }

    fn edge(from: &str, to: &str, symbol: &str, kind: EdgeKind) -> CouplingEdge {
        CouplingEdge {
            from: ModulePath::new(from),
            to: ModulePath::new(to),
            symbol: symbol.to_owned(),
            kind,
        }
    }

    #[test]
    fn unreferenced_item_is_dead() {
        let items = vec![item("crate::a", "Foo", PublicItemKind::Struct)];
        let dead = find_dead_pub_items(items, &[]);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].name, "Foo");
    }

    #[test]
    fn directly_referenced_item_is_alive() {
        let items = vec![item("crate::a", "Foo", PublicItemKind::Struct)];
        let edges = vec![edge("crate::b", "crate::a", "Foo", EdgeKind::Use)];
        assert!(find_dead_pub_items(items, &edges).is_empty());
    }

    #[test]
    fn glob_import_keeps_every_item_in_target_module_alive() {
        let items = vec![
            item("crate::a", "Foo", PublicItemKind::Struct),
            item("crate::a", "Bar", PublicItemKind::Fn),
            item("crate::a", "Baz", PublicItemKind::Const),
        ];
        let edges = vec![edge("crate::b", "crate::a", "*", EdgeKind::Use)];
        assert!(find_dead_pub_items(items, &edges).is_empty());
    }

    #[test]
    fn glob_import_does_not_revive_items_in_other_modules() {
        let items = vec![
            item("crate::a", "Foo", PublicItemKind::Struct),
            item("crate::b", "Bar", PublicItemKind::Fn),
        ];
        let edges = vec![edge("crate::c", "crate::a", "*", EdgeKind::Use)];
        let dead = find_dead_pub_items(items, &edges);
        assert_eq!(dead.len(), 1);
        assert_eq!(dead[0].name, "Bar");
    }

    #[test]
    fn associated_function_call_keeps_enclosing_item_alive() {
        // `Foo::bar()` from another module resolves with longest-prefix
        // matching to `(crate::a, "Foo::bar")`. That should keep the
        // `Foo` struct alive even though `bar` isn't tracked
        // independently here.
        let items = vec![item("crate::a", "Foo", PublicItemKind::Struct)];
        let edges = vec![edge("crate::b", "crate::a", "Foo::bar", EdgeKind::Call)];
        assert!(find_dead_pub_items(items, &edges).is_empty());
    }

    #[test]
    fn self_reference_does_not_keep_item_alive() {
        // An item only ever referenced inside its own module is still
        // "dead" from an API surface perspective — nobody outside the
        // module relies on it.
        let items = vec![item("crate::a", "Foo", PublicItemKind::Struct)];
        let edges = vec![edge("crate::a", "crate::a", "Foo", EdgeKind::Type)];
        let dead = find_dead_pub_items(items, &edges);
        assert_eq!(dead.len(), 1);
    }

    #[test]
    fn unrelated_symbol_does_not_count_as_reference() {
        let items = vec![item("crate::a", "Foo", PublicItemKind::Struct)];
        // `Foobar` is not `Foo` and isn't `Foo::…`.
        let edges = vec![edge("crate::b", "crate::a", "Foobar", EdgeKind::Type)];
        let dead = find_dead_pub_items(items, &edges);
        assert_eq!(dead.len(), 1);
    }

    #[test]
    fn at_crate_root_is_true_for_top_level_items_only() {
        assert!(item("crate", "X", PublicItemKind::Fn).at_crate_root());
        assert!(!item("crate::a", "X", PublicItemKind::Fn).at_crate_root());
    }

    #[test]
    fn dead_items_are_sorted_by_module_then_name() {
        let items = vec![
            item("crate::b", "Zeta", PublicItemKind::Fn),
            item("crate::b", "Alpha", PublicItemKind::Fn),
            item("crate::a", "Mid", PublicItemKind::Fn),
        ];
        let dead = find_dead_pub_items(items, &[]);
        let order: Vec<(&str, &str)> = dead
            .iter()
            .map(|d| (d.module.as_str(), d.name.as_str()))
            .collect();
        assert_eq!(
            order,
            vec![
                ("crate::a", "Mid"),
                ("crate::b", "Alpha"),
                ("crate::b", "Zeta"),
            ]
        );
    }

    #[test]
    fn public_item_kind_as_str_round_trips() {
        for kind in [
            PublicItemKind::Fn,
            PublicItemKind::Struct,
            PublicItemKind::Enum,
            PublicItemKind::Trait,
            PublicItemKind::TypeAlias,
            PublicItemKind::Const,
            PublicItemKind::Static,
            PublicItemKind::Union,
            PublicItemKind::Macro,
            PublicItemKind::Mod,
        ] {
            assert!(!kind.as_str().is_empty());
        }
    }
}

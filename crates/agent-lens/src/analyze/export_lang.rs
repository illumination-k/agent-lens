//! What the export-status languages have in common.
//!
//! Two analyzers judge a declaration by how far it reaches — `analyze
//! visibility` (is this `pub` wider than its callers need?) and `analyze
//! unreachable` (can anything reach this at all?) — and both can only do
//! so where the adapter extracts export status, which today means Rust
//! and Go. [`ExportLang`] is that per-node question plus the handful of
//! per-language conventions the answer depends on.
//!
//! [`InterfaceIndex`] is the second thing they share: a Go method whose
//! name and arity match an interface declared in the analyzed tree — or
//! a well-known standard-library one such as `fmt.Stringer` — can be
//! called through that interface, and no static edge records it. Both
//! analyzers have to stop trusting "no caller" for such a method, so the
//! matching rule lives here rather than in either of them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use lens_domain::{InterfaceShape, UbiquitousMethodNames};

use super::SourceLang;
use super::call_graph::model::{CallGraphNode, GraphLanguage, NodeVisibility};

/// The two languages whose adapters extract export status. Everything
/// else is counted as skipped instead of being judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportLang {
    /// Private items are visible in the defining module *and its
    /// descendants*, and the intermediate `pub(in …)` scopes exist.
    Rust,
    /// The package is the only boundary: a caller one directory down is
    /// as external as one in another repository.
    Go,
}

impl ExportLang {
    pub(crate) fn of(node: &CallGraphNode) -> Option<Self> {
        match SourceLang::from_path(Path::new(&node.file)) {
            Some(SourceLang::Rust) => Some(Self::Rust),
            Some(SourceLang::Go) => Some(Self::Go),
            _ => None,
        }
    }

    /// The visibility that exposes a function beyond its own compilation
    /// unit in this language.
    pub(crate) fn public(self) -> NodeVisibility {
        match self {
            Self::Rust => NodeVisibility::Public,
            Self::Go => NodeVisibility::Exported,
        }
    }

    /// The visibility that confines a function to its own compilation
    /// unit — the only one a "nothing can reach this" verdict is decided
    /// on, since anything wider can be reached from outside the
    /// analyzed tree.
    pub(crate) fn private(self) -> NodeVisibility {
        match self {
            Self::Rust => NodeVisibility::Private,
            Self::Go => NodeVisibility::Unexported,
        }
    }

    /// Whether a scope module also covers the modules below it. Rust
    /// visibility is inherited downward; Go packages are flat.
    pub(crate) fn scope_covers_descendants(self) -> bool {
        matches!(self, Self::Rust)
    }

    /// The method names this language's resolver refuses to attribute
    /// from a receiver call alone.
    pub(crate) fn ubiquitous_names(self) -> UbiquitousMethodNames {
        match self {
            Self::Rust => GraphLanguage::Rust.ubiquitous_method_names(),
            Self::Go => GraphLanguage::Go.ubiquitous_method_names(),
        }
    }
}

/// Standard-library interfaces a Go method satisfies by name and
/// parameter count alone, as (method, parameter count, interface).
///
/// These are declared outside any analyzed tree, so [`InterfaceIndex`]
/// cannot learn them from the source, yet they are exactly the methods
/// that are exported so packages the analyzer never sees can call them —
/// `fmt` through `%v` / `%#v`, `encoding/*` through reflection, `io`
/// and `net/http` through interface values. A method matching one is
/// treated like a method matching an in-tree interface. The match is
/// structural and can be coincidental (`Close`, `Len`), the same
/// over-approximation the in-tree rule accepts.
const STDLIB_INTERFACE_METHODS: &[(&str, usize, &str)] = &[
    ("As", 1, "errors.As"),
    ("Close", 0, "io.Closer"),
    ("Error", 0, "error"),
    ("Format", 2, "fmt.Formatter"),
    ("GoString", 0, "fmt.GoStringer"),
    ("Is", 1, "errors.Is"),
    ("Len", 0, "sort.Interface"),
    ("Less", 2, "sort.Interface"),
    ("LogValue", 0, "log/slog.LogValuer"),
    ("MarshalBinary", 0, "encoding.BinaryMarshaler"),
    ("MarshalJSON", 0, "encoding/json.Marshaler"),
    ("MarshalText", 0, "encoding.TextMarshaler"),
    ("MarshalXML", 2, "encoding/xml.Marshaler"),
    ("Pop", 0, "container/heap.Interface"),
    ("Push", 1, "container/heap.Interface"),
    ("Read", 1, "io.Reader"),
    ("ReadAt", 2, "io.ReaderAt"),
    ("ReadByte", 0, "io.ByteReader"),
    ("ReadFrom", 1, "io.ReaderFrom"),
    ("ReadRune", 0, "io.RuneReader"),
    ("Scan", 1, "database/sql.Scanner"),
    ("Scan", 2, "fmt.Scanner"),
    ("Seek", 2, "io.Seeker"),
    ("ServeHTTP", 2, "net/http.Handler"),
    ("Set", 1, "flag.Value"),
    ("String", 0, "fmt.Stringer"),
    ("Swap", 2, "sort.Interface"),
    ("UnmarshalBinary", 1, "encoding.BinaryUnmarshaler"),
    ("UnmarshalJSON", 1, "encoding/json.Unmarshaler"),
    ("UnmarshalText", 1, "encoding.TextUnmarshaler"),
    ("UnmarshalXML", 2, "encoding/xml.Unmarshaler"),
    ("Unwrap", 0, "errors.Unwrap"),
    ("Value", 0, "database/sql/driver.Valuer"),
    ("Write", 1, "io.Writer"),
    ("WriteAt", 2, "io.WriterAt"),
    ("WriteByte", 1, "io.ByteWriter"),
    ("WriteString", 1, "io.StringWriter"),
    ("WriteTo", 1, "io.WriterTo"),
];

/// Method sets of the interfaces declared in the analyzed tree, plus
/// the well-known standard-library ones in [`STDLIB_INTERFACE_METHODS`],
/// keyed for the structural question every Go method gets asked: could
/// its calls dispatch through an interface?
pub(crate) struct InterfaceIndex {
    /// Method name → parameter count → interfaces declaring such a
    /// method, by qualified name, sorted.
    by_method: BTreeMap<String, BTreeMap<usize, BTreeSet<String>>>,
}

impl InterfaceIndex {
    pub(crate) fn new(interfaces: &[InterfaceShape]) -> Self {
        let mut by_method: BTreeMap<String, BTreeMap<usize, BTreeSet<String>>> = BTreeMap::new();
        for &(method, param_count, interface) in STDLIB_INTERFACE_METHODS {
            by_method
                .entry(method.to_owned())
                .or_default()
                .entry(param_count)
                .or_default()
                .insert(interface.to_owned());
        }
        for interface in interfaces {
            let Some(name) = interface.qualified_name.known_value() else {
                continue;
            };
            for method in &interface.methods {
                by_method
                    .entry(method.name.clone())
                    .or_default()
                    .entry(method.param_count)
                    .or_default()
                    .insert(name.clone());
            }
        }
        Self { by_method }
    }

    /// Interfaces this node could satisfy: a method (never a free
    /// function — only method sets satisfy interfaces) matching one of
    /// their methods by name and parameter count. Go only, because the
    /// method sets come from Go declarations; a Rust method sharing a
    /// name with one is no dispatch candidate.
    pub(crate) fn matching(&self, node: &CallGraphNode, lang: ExportLang) -> Vec<String> {
        if lang != ExportLang::Go || node.impl_owner.is_none() {
            return Vec::new();
        }
        let Some(param_count) = node.param_count else {
            return Vec::new();
        };
        self.by_method
            .get(&node.name)
            .and_then(|by_arity| by_arity.get(&param_count))
            .map(|names| names.iter().cloned().collect())
            .unwrap_or_default()
    }
}

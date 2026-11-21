//! Serde model of the JSON IR `quint parse --out` writes.
//!
//! Only the parts the analyzers read are modelled; serde drops every
//! other field. Declaration kinds this adapter has no use for collapse
//! into [`Decl::Other`] so a newer Quint that adds one still parses.

use serde::Deserialize;

/// Node id. Every expression, declaration and module carries one, and
/// the source map and the name-resolution table are keyed by it.
pub type Id = u64;

/// Top-level `quint parse` output.
#[derive(Debug, Clone, Deserialize)]
pub struct ParseOutput {
    #[serde(default)]
    pub modules: Vec<Module>,
    /// Name-use id → the definition it resolves to.
    #[serde(default)]
    pub table: std::collections::HashMap<String, TableEntry>,
    #[serde(default)]
    pub errors: Vec<QuintDiagnostic>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QuintDiagnostic {
    pub explanation: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TableEntry {
    pub id: Id,
}

/// One `module Name { ... }`.
#[derive(Debug, Clone, Deserialize)]
pub struct Module {
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub declarations: Vec<Decl>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Decl {
    Def(OpDef),
    Import(ModuleRef),
    Export(ModuleRef),
    Instance(ModuleRef),
    #[serde(other)]
    Other,
}

/// The module-referencing declarations: `import`, `export`, and
/// instance (`import M(N = 1) as I`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModuleRef {
    pub id: Id,
    pub proto_name: String,
    /// The single imported name, or `"*"`; absent on instances.
    #[serde(default)]
    pub def_name: Option<String>,
    /// `from "./file"`; absent when the module lives in the same file.
    #[serde(default)]
    pub from_source: Option<String>,
}

/// An operator definition: `def`, `pure def`, `action`, `val`,
/// `temporal`, `run`, and `nondet` / `val` bindings inside `let`.
#[derive(Debug, Clone, Deserialize)]
pub struct OpDef {
    pub id: Id,
    pub name: String,
    pub qualifier: String,
    pub expr: Expr,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Param {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Expr {
    Name {
        id: Id,
        name: String,
    },
    Bool {
        id: Id,
        value: bool,
    },
    /// Quint integers are unbounded; keep the literal as JSON so a big
    /// one does not fail the parse.
    Int {
        id: Id,
        value: serde_json::Value,
    },
    Str {
        id: Id,
        value: String,
    },
    App {
        id: Id,
        opcode: String,
        #[serde(default)]
        args: Vec<Expr>,
    },
    Lambda {
        id: Id,
        params: Vec<Param>,
        expr: Box<Expr>,
    },
    Let {
        id: Id,
        opdef: Box<OpDef>,
        expr: Box<Expr>,
    },
}

impl Expr {
    pub fn id(&self) -> Id {
        match self {
            Self::Name { id, .. }
            | Self::Bool { id, .. }
            | Self::Int { id, .. }
            | Self::Str { id, .. }
            | Self::App { id, .. }
            | Self::Lambda { id, .. }
            | Self::Let { id, .. } => *id,
        }
    }

    /// Visit this expression and every sub-expression, pre-order.
    pub fn walk(&self, visit: &mut dyn FnMut(&Expr)) {
        visit(self);
        match self {
            Self::App { args, .. } => args.iter().for_each(|a| a.walk(visit)),
            Self::Lambda { expr, .. } => expr.walk(visit),
            Self::Let { opdef, expr, .. } => {
                opdef.expr.walk(visit);
                expr.walk(visit);
            }
            Self::Name { .. } | Self::Bool { .. } | Self::Int { .. } | Self::Str { .. } => {}
        }
    }
}

/// `{"sourceIndex": {"0": "/abs/a.qnt"}, "map": {"<id>": [src, start, end]}}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceMap {
    pub source_index: std::collections::HashMap<String, String>,
    pub map: std::collections::HashMap<String, (serde_json::Value, Pos, Pos)>,
}

/// Zero-based line / column as `quint` reports them.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pos {
    pub line: usize,
}

//! Per-function taint flow extraction for `analyze taint`.
//!
//! Lowers every function the call graph knows (free fns, `impl` methods,
//! trait default methods) into a [`FunctionFlow`]. Each body is first
//! read into a small value language ([`Ir`]): a variable, a call's
//! result, what flows into a call, or a union of those. Every `let`,
//! assignment, `for`, `if let` and `match` arm then becomes a write of
//! one of those values into the names its pattern binds, and the writes
//! are iterated to a fixpoint — flow-insensitive, so a variable is
//! tainted if any write to it is. A block's value is its tail
//! expression, so `if`/`match` results and a body's implicit return are
//! followed like explicit `return`s.
//!
//! Macro arguments are read as expressions the way the call index reads
//! them (`format!`, `vec!`, `println!`), plus the names a format string
//! captures inline (`format!("{name}")`), so a query built with
//! `format!` carries its inputs.
//!
//! Sources are parameters whose type resolves, through the file's `use`
//! items, to a request extractor of a known web framework
//! ([`DEFAULT_SOURCE_TYPES`]). Sinks and sanitizers on paths match the
//! resolved path (`Command::new` under `use std::process::Command` is
//! `std::process::Command::new`); the SQL driver methods whose receiver
//! type a syntax-only pass cannot know match by name and argument count.

use std::collections::{BTreeSet, HashMap, HashSet};

use lens_domain::{ArgSelector, FlowCall, FunctionFlow, Origin, SinkSpec, TaintSource};
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{
    BinOp, Block, Expr, ExprCall, ExprMethodCall, FnArg, ImplItem, Item, ItemUse, Lit, Macro, Pat,
    Signature, Stmt, Type, UnOp,
};

use crate::call_index::{macro_argument_exprs, walk_use_tree};
use crate::common::{WalkOptions, render_tokens, split_guard, walk_fn_items};
use crate::parser::RustParseError;

/// Parameter types treated as untrusted input, as fully qualified paths.
/// A reference to one counts too, and so does any generic instantiation
/// (`Query<Params>`).
pub const DEFAULT_SOURCE_TYPES: &[&str] = &[
    "axum::extract::Query",
    "axum::extract::Path",
    "axum::extract::Form",
    "axum::extract::Json",
    "axum::extract::RawQuery",
    "axum::extract::Request",
    "axum::Form",
    "axum::Json",
    "axum::http::HeaderMap",
    "axum::http::Request",
    "axum_extra::extract::Query",
    "http::HeaderMap",
    "http::Request",
    "hyper::Request",
    "actix_web::HttpRequest",
    "actix_web::web::Query",
    "actix_web::web::Path",
    "actix_web::web::Json",
    "actix_web::web::Form",
    "rocket::form::Form",
    "rocket::serde::json::Json",
];

/// Path sinks: `(resolved path, kind, checked arguments)`.
const PATH_SINKS: &[(&str, &str, ArgSelector)] = &[
    (
        "std::process::Command::new",
        "command-injection",
        ArgSelector::At(0),
    ),
    (
        "tokio::process::Command::new",
        "command-injection",
        ArgSelector::At(0),
    ),
    ("sqlx::query", "sql-injection", ArgSelector::At(0)),
    ("sqlx::query_as", "sql-injection", ArgSelector::At(0)),
    ("sqlx::query_scalar", "sql-injection", ArgSelector::At(0)),
    ("sqlx::raw_sql", "sql-injection", ArgSelector::At(0)),
    ("diesel::sql_query", "sql-injection", ArgSelector::At(0)),
    ("diesel::dsl::sql", "sql-injection", ArgSelector::At(0)),
    ("std::fs::read", "path-traversal", ArgSelector::At(0)),
    (
        "std::fs::read_to_string",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("std::fs::read_dir", "path-traversal", ArgSelector::At(0)),
    ("std::fs::write", "path-traversal", ArgSelector::At(0)),
    ("std::fs::remove_file", "path-traversal", ArgSelector::At(0)),
    ("std::fs::remove_dir", "path-traversal", ArgSelector::At(0)),
    (
        "std::fs::remove_dir_all",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("std::fs::create_dir", "path-traversal", ArgSelector::At(0)),
    (
        "std::fs::create_dir_all",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("std::fs::copy", "path-traversal", ArgSelector::All),
    ("std::fs::rename", "path-traversal", ArgSelector::All),
    ("std::fs::File::open", "path-traversal", ArgSelector::At(0)),
    (
        "std::fs::File::create",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("tokio::fs::read", "path-traversal", ArgSelector::At(0)),
    (
        "tokio::fs::read_to_string",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("tokio::fs::read_dir", "path-traversal", ArgSelector::At(0)),
    ("tokio::fs::write", "path-traversal", ArgSelector::At(0)),
    (
        "tokio::fs::remove_file",
        "path-traversal",
        ArgSelector::At(0),
    ),
    (
        "tokio::fs::remove_dir_all",
        "path-traversal",
        ArgSelector::At(0),
    ),
    (
        "tokio::fs::create_dir_all",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("tokio::fs::copy", "path-traversal", ArgSelector::All),
    ("tokio::fs::rename", "path-traversal", ArgSelector::All),
    (
        "tokio::fs::File::open",
        "path-traversal",
        ArgSelector::At(0),
    ),
    (
        "tokio::fs::File::create",
        "path-traversal",
        ArgSelector::At(0),
    ),
    (
        "actix_files::NamedFile::open",
        "path-traversal",
        ArgSelector::At(0),
    ),
    ("reqwest::get", "ssrf", ArgSelector::At(0)),
    ("reqwest::blocking::get", "ssrf", ArgSelector::At(0)),
    ("ureq::get", "ssrf", ArgSelector::At(0)),
    ("ureq::post", "ssrf", ArgSelector::At(0)),
    ("ureq::put", "ssrf", ArgSelector::At(0)),
    ("ureq::delete", "ssrf", ArgSelector::At(0)),
    ("std::net::TcpStream::connect", "ssrf", ArgSelector::At(0)),
    ("tokio::net::TcpStream::connect", "ssrf", ArgSelector::At(0)),
    (
        "axum::response::Redirect::to",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "axum::response::Redirect::temporary",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "axum::response::Redirect::permanent",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "rocket::response::Redirect::to",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "rocket::response::Redirect::found",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "rocket::response::Redirect::temporary",
        "open-redirect",
        ArgSelector::At(0),
    ),
    (
        "rocket::response::Redirect::permanent",
        "open-redirect",
        ArgSelector::At(0),
    ),
    ("axum::response::Html", "xss", ArgSelector::At(0)),
    ("maud::PreEscaped", "xss", ArgSelector::At(0)),
];

/// Method sinks matched by name on calls the call graph leaves
/// unresolved: `(method, kind, checked arguments, fewest arguments)`.
/// The SQL methods are those of `rusqlite`, `tokio-postgres` and
/// `postgres`, where the query text comes first; the argument floor keeps
/// out same-named methods that take no query (`sqlx`'s
/// `query.execute(&pool)`, `reqwest`'s `.query(&params)`). `arg`/`args`
/// are `Command`'s builder.
const METHOD_SINKS: &[(&str, &str, ArgSelector, usize)] = &[
    ("arg", "command-injection", ArgSelector::At(0), 1),
    ("args", "command-injection", ArgSelector::At(0), 1),
    ("execute", "sql-injection", ArgSelector::At(0), 2),
    ("query", "sql-injection", ArgSelector::At(0), 2),
    ("query_row", "sql-injection", ArgSelector::At(0), 2),
    ("query_one", "sql-injection", ArgSelector::At(0), 2),
    ("query_opt", "sql-injection", ArgSelector::At(0), 2),
    ("prepare", "sql-injection", ArgSelector::At(0), 1),
    ("prepare_cached", "sql-injection", ArgSelector::At(0), 1),
    ("execute_batch", "sql-injection", ArgSelector::At(0), 1),
    ("batch_execute", "sql-injection", ArgSelector::At(0), 1),
    ("simple_query", "sql-injection", ArgSelector::At(0), 1),
];

/// Path functions whose result carries none of their input's taint:
/// escaping and quoting.
const PATH_SANITIZERS: &[&str] = &[
    "urlencoding::encode",
    "html_escape::encode_text",
    "html_escape::encode_safe",
    "html_escape::encode_double_quoted_attribute",
    "html_escape::encode_single_quoted_attribute",
    "ammonia::clean",
    "shell_escape::escape",
    "shell_words::quote",
];

/// Methods whose result is a number or a boolean.
const CLEAN_METHODS: &[&str] = &[
    "len",
    "is_empty",
    "count",
    "contains",
    "contains_key",
    "starts_with",
    "ends_with",
    "is_some",
    "is_none",
    "is_ok",
    "is_err",
    "eq",
    "ne",
    "cmp",
    "partial_cmp",
    "exists",
    "is_file",
    "is_dir",
];

/// Types whose values cannot carry an injection payload.
const CLEAN_TYPES: &[&str] = &[
    "bool", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
    "f32", "f64",
];

/// Lower every function of `source` into a [`FunctionFlow`].
/// `extra_source_types` extends [`DEFAULT_SOURCE_TYPES`] with more fully
/// qualified type paths.
pub fn extract_taint_flows(
    source: &str,
    extra_source_types: &[String],
) -> Result<Vec<FunctionFlow>, RustParseError> {
    let file = syn::parse_file(source)?;
    let mut uses = UseCollector::default();
    uses.visit_file(&file);
    let source_types: Vec<&str> = DEFAULT_SOURCE_TYPES
        .iter()
        .copied()
        .chain(extra_source_types.iter().map(String::as_str))
        .collect();
    let context = FileContext {
        uses: uses.aliases,
        source_types,
    };
    let mut out = Vec::new();
    walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
        out.push(Lowering::lower(&context, site.sig, site.block));
    });
    Ok(out)
}

/// Every `use` in the file, nested ones included, as alias → target
/// segments. One file-wide table: two scopes binding the same name to
/// different paths is rare enough to lose.
#[derive(Default)]
struct UseCollector {
    aliases: HashMap<String, Vec<String>>,
}

impl<'ast> Visit<'ast> for UseCollector {
    fn visit_item_use(&mut self, item: &'ast ItemUse) {
        let mut leaves = Vec::new();
        walk_use_tree(&item.tree, &mut Vec::new(), &mut leaves);
        for (alias, target) in leaves {
            if alias != "*" {
                self.aliases.insert(alias, target);
            }
        }
    }
}

struct FileContext<'a> {
    uses: HashMap<String, Vec<String>>,
    source_types: Vec<&'a str>,
}

impl FileContext<'_> {
    /// `path` with its first segment replaced by what a `use` binds it to.
    fn resolve(&self, path: &syn::Path) -> String {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        let Some((first, rest)) = segments.split_first() else {
            return String::new();
        };
        match self
            .uses
            .get(first)
            .filter(|_| path.leading_colon.is_none())
        {
            Some(target) => target
                .iter()
                .chain(rest)
                .cloned()
                .collect::<Vec<_>>()
                .join("::"),
            None => segments.join("::"),
        }
    }

    fn is_source_type(&self, ty: &Type) -> bool {
        type_path(ty).is_some_and(|path| {
            let resolved = self.resolve(path);
            self.source_types.contains(&resolved.as_str())
        })
    }
}

/// The path of a (possibly referenced) path type.
fn type_path(ty: &Type) -> Option<&syn::Path> {
    match ty {
        Type::Path(path) => Some(&path.path),
        Type::Reference(reference) => type_path(&reference.elem),
        _ => None,
    }
}

fn is_clean_type(ty: &Type) -> bool {
    type_path(ty).is_some_and(|path| {
        path.segments.len() == 1
            && path
                .segments
                .first()
                .is_some_and(|segment| CLEAN_TYPES.contains(&segment.ident.to_string().as_str()))
    })
}

/// A value inside one body, before the fixpoint gives it origins.
#[derive(Debug, Clone, Default)]
enum Ir {
    #[default]
    Empty,
    Var(String),
    Call(usize),
    Inputs(usize),
    Many(Vec<Ir>),
}

/// `value`, unless the operation yields a clean type (`!x`, `x as u32`).
fn clean_if(clean: bool, value: Ir) -> Ir {
    if clean { Ir::Empty } else { value }
}

fn many(items: impl IntoIterator<Item = Ir>) -> Ir {
    let items: Vec<Ir> = items
        .into_iter()
        .filter(|item| !matches!(item, Ir::Empty))
        .collect();
    if items.is_empty() {
        Ir::Empty
    } else {
        Ir::Many(items)
    }
}

/// One call site, its operands still in [`Ir`].
struct CallDraft {
    line: usize,
    callee_name: Option<String>,
    label: String,
    receiver: Ir,
    arguments: Vec<Ir>,
    method_syntax: bool,
    sanitizer: bool,
    sink: Option<SinkSpec>,
}

struct Lowering<'a> {
    file: &'a FileContext<'a>,
    env: HashMap<String, BTreeSet<Origin>>,
    writes: Vec<(String, Ir)>,
    calls: Vec<CallDraft>,
    returns: Vec<Ir>,
    sources: Vec<TaintSource>,
    /// Parameter and receiver names: a write into a field of one does
    /// not taint it (see the Go adapter for why).
    handles: HashSet<String>,
    /// Names pinned to a constant by an enclosing allowlist check.
    pinned: Vec<String>,
    /// Depth of closures and nested `fn`s: their `return`s are not ours.
    nested: usize,
}

impl<'a> Lowering<'a> {
    fn lower(file: &'a FileContext<'a>, sig: &Signature, block: &Block) -> FunctionFlow {
        let mut lowering = Self {
            file,
            env: HashMap::new(),
            writes: Vec::new(),
            calls: Vec::new(),
            returns: Vec::new(),
            sources: Vec::new(),
            handles: HashSet::new(),
            pinned: Vec::new(),
            nested: 0,
        };
        let takes_receiver = lowering.bind_signature(sig);
        let tail = lowering.block(block);
        lowering.returns.push(tail);
        lowering.propagate();
        let calls = lowering
            .calls
            .iter()
            .map(|draft| lowering.flow_call(draft))
            .collect();
        let returns = lowering
            .returns
            .iter()
            .flat_map(|ir| lowering.eval(ir))
            .collect();
        FunctionFlow {
            start_line: sig.span().start().line,
            takes_receiver,
            sources: lowering.sources,
            calls,
            returns,
        }
    }

    /// Bind each typed parameter to its slot, or to a fresh source when
    /// its type is a request extractor; `Query(params): Query<P>` binds
    /// `params`. Returns whether the signature takes `self`.
    fn bind_signature(&mut self, sig: &Signature) -> bool {
        let mut takes_receiver = false;
        let mut slot = 0;
        for input in &sig.inputs {
            let pat_type = match input {
                FnArg::Receiver(_) => {
                    takes_receiver = true;
                    self.handles.insert("self".to_owned());
                    continue;
                }
                FnArg::Typed(pat_type) => pat_type,
            };
            let names = pattern_names(&pat_type.pat);
            self.handles.extend(names.iter().cloned());
            let origin = if self.file.is_source_type(&pat_type.ty) {
                self.sources.push(TaintSource {
                    label: compact_tokens(&format!(
                        "{}: {}",
                        render_tokens(&pat_type.pat),
                        render_tokens(&pat_type.ty)
                    )),
                    line: pat_type.span().start().line,
                });
                Some(Origin::Source(self.sources.len() - 1))
            } else {
                (!is_clean_type(&pat_type.ty)).then_some(Origin::Param(slot))
            };
            if let Some(origin) = origin {
                for name in names {
                    self.env.entry(name).or_default().insert(origin);
                }
            }
            slot += 1;
        }
        takes_receiver
    }

    /// Walk a block; its value is its tail expression's.
    fn block(&mut self, block: &Block) -> Ir {
        let mut tail = Ir::Empty;
        for stmt in &block.stmts {
            tail = self.stmt(stmt);
        }
        tail
    }

    /// Walk one statement; its value is what it would contribute as a
    /// block's tail — nothing once a `;` ends it.
    fn stmt(&mut self, stmt: &Stmt) -> Ir {
        match stmt {
            Stmt::Local(local) => {
                let value = local.init.as_ref().map_or(Ir::Empty, |init| {
                    if let Some((_, diverge)) = &init.diverge {
                        self.expr(diverge);
                    }
                    self.expr(&init.expr)
                });
                self.bind_pattern(&local.pat, &value);
                Ir::Empty
            }
            Stmt::Item(item) => {
                self.nested_item(item);
                Ir::Empty
            }
            Stmt::Expr(expr, semi) => {
                let value = self.expr(expr);
                clean_if(semi.is_some(), value)
            }
            Stmt::Macro(mac) => {
                let value = self.mac(&mac.mac);
                clean_if(mac.semi_token.is_some(), value)
            }
        }
    }

    /// A `fn` (or an `impl`'s methods) declared inside the body is no
    /// call-graph node: its calls belong to this function, as the call
    /// index attributes them.
    fn nested_item(&mut self, item: &Item) {
        self.nested += 1;
        match item {
            Item::Fn(item_fn) => {
                self.block(&item_fn.block);
            }
            Item::Impl(item_impl) => {
                for impl_item in &item_impl.items {
                    if let ImplItem::Fn(method) = impl_item {
                        self.block(&method.block);
                    }
                }
            }
            _ => {}
        }
        self.nested -= 1;
    }

    fn expr(&mut self, expr: &Expr) -> Ir {
        match expr {
            Expr::Path(path) => match single_ident(path) {
                Some(name) if !self.pinned.contains(&name) => Ir::Var(name),
                _ => Ir::Empty,
            },
            Expr::Call(call) => self.call(call),
            Expr::MethodCall(call) => self.method_call(call),
            Expr::Macro(mac) => self.mac(&mac.mac),
            Expr::Binary(binary) => self.binary(binary),
            Expr::Assign(assign) => {
                let value = self.expr(&assign.right);
                self.assign(&assign.left, value);
                Ir::Empty
            }
            Expr::Unary(unary) => {
                let inner = self.expr(&unary.expr);
                clean_if(matches!(unary.op, UnOp::Not(_)), inner)
            }
            Expr::Cast(cast) => {
                let inner = self.expr(&cast.expr);
                clean_if(is_clean_type(&cast.ty), inner)
            }
            Expr::Reference(reference) => self.expr(&reference.expr),
            Expr::Paren(paren) => self.expr(&paren.expr),
            Expr::Try(try_expr) => self.expr(&try_expr.expr),
            Expr::Await(await_expr) => self.expr(&await_expr.base),
            Expr::Field(field) => self.expr(&field.base),
            Expr::Index(index) => {
                self.expr(&index.index);
                self.expr(&index.expr)
            }
            Expr::Tuple(_)
            | Expr::Array(_)
            | Expr::Struct(_)
            | Expr::Repeat(_)
            | Expr::Range(_) => self.aggregate(expr),
            Expr::Block(block) => self.block(&block.block),
            Expr::Unsafe(block) => self.block(&block.block),
            Expr::Async(block) => self.block(&block.block),
            Expr::If(if_expr) => self.if_expr(if_expr),
            Expr::Let(let_expr) => {
                let value = self.expr(&let_expr.expr);
                self.bind_pattern(&let_expr.pat, &value);
                Ir::Empty
            }
            Expr::Match(match_expr) => self.match_expr(match_expr),
            Expr::ForLoop(_)
            | Expr::While(_)
            | Expr::Loop(_)
            | Expr::Break(_)
            | Expr::Return(_) => {
                self.statement_expr(expr);
                Ir::Empty
            }
            // A closure's value is what its body reads: `map(|s| ..)`
            // feeds the iterator chain whatever the closure captures.
            Expr::Closure(closure) => {
                self.nested += 1;
                let body = self.expr(&closure.body);
                self.nested -= 1;
                body
            }
            _ => Ir::Empty,
        }
    }

    fn binary(&mut self, binary: &syn::ExprBinary) -> Ir {
        let left = self.expr(&binary.left);
        let right = self.expr(&binary.right);
        match binary.op {
            BinOp::AddAssign(_)
            | BinOp::SubAssign(_)
            | BinOp::MulAssign(_)
            | BinOp::DivAssign(_)
            | BinOp::RemAssign(_)
            | BinOp::BitXorAssign(_)
            | BinOp::BitAndAssign(_)
            | BinOp::BitOrAssign(_)
            | BinOp::ShlAssign(_)
            | BinOp::ShrAssign(_) => {
                self.assign(&binary.left, right);
                Ir::Empty
            }
            BinOp::Add(_)
            | BinOp::Sub(_)
            | BinOp::Mul(_)
            | BinOp::Div(_)
            | BinOp::Rem(_)
            | BinOp::BitXor(_)
            | BinOp::BitAnd(_)
            | BinOp::BitOr(_)
            | BinOp::Shl(_)
            | BinOp::Shr(_) => many([left, right]),
            // Comparisons and logic yield a bool.
            _ => Ir::Empty,
        }
    }

    /// Tuples, arrays, struct literals, repeats and ranges carry what
    /// their parts carry.
    fn aggregate(&mut self, expr: &Expr) -> Ir {
        let parts: Vec<&Expr> = match expr {
            Expr::Tuple(tuple) => tuple.elems.iter().collect(),
            Expr::Array(array) => array.elems.iter().collect(),
            Expr::Struct(literal) => literal
                .fields
                .iter()
                .map(|field| &field.expr)
                .chain(literal.rest.as_deref())
                .collect(),
            Expr::Repeat(repeat) => vec![repeat.len.as_ref(), repeat.expr.as_ref()],
            Expr::Range(range) => range
                .start
                .as_deref()
                .into_iter()
                .chain(range.end.as_deref())
                .collect(),
            _ => Vec::new(),
        };
        let items: Vec<Ir> = parts.into_iter().map(|part| self.expr(part)).collect();
        many(items)
    }

    fn if_expr(&mut self, if_expr: &syn::ExprIf) -> Ir {
        self.expr(&if_expr.cond);
        let pins = self.pinned_by(&if_expr.cond);
        let depth = self.pinned.len();
        self.pinned.extend(pins);
        let then = self.block(&if_expr.then_branch);
        self.pinned.truncate(depth);
        let otherwise = if_expr
            .else_branch
            .as_ref()
            .map_or(Ir::Empty, |(_, otherwise)| self.expr(otherwise));
        many([then, otherwise])
    }

    /// Each arm's pattern binds from the scrutinee; an arm matching only
    /// constants pins the scrutinee's variable inside it.
    fn match_expr(&mut self, match_expr: &syn::ExprMatch) -> Ir {
        let scrutinee = self.expr(&match_expr.expr);
        let subject = pin_subject(&match_expr.expr);
        let mut arms = Vec::new();
        for arm in &match_expr.arms {
            let (pat, guard) = split_guard(&arm.pat);
            self.bind_pattern(pat, &scrutinee);
            let depth = self.pinned.len();
            if let Some(subject) = subject.as_ref().filter(|_| pattern_is_constant(pat)) {
                self.pinned.push(subject.clone());
            }
            if let Some(guard) = guard {
                self.expr(guard);
            }
            arms.push(self.expr(&arm.body));
            self.pinned.truncate(depth);
        }
        many(arms)
    }

    /// Loops and jumps: no value of their own, but `for` binds its
    /// pattern and `return` records the function's result.
    fn statement_expr(&mut self, expr: &Expr) {
        match expr {
            Expr::ForLoop(for_loop) => {
                let items = self.expr(&for_loop.expr);
                self.bind_pattern(&for_loop.pat, &items);
                self.block(&for_loop.body);
            }
            Expr::While(while_loop) => {
                self.expr(&while_loop.cond);
                self.block(&while_loop.body);
            }
            Expr::Loop(loop_expr) => {
                self.block(&loop_expr.body);
            }
            Expr::Break(break_expr) => {
                if let Some(value) = &break_expr.expr {
                    self.expr(value);
                }
            }
            Expr::Return(return_expr) => {
                let value = return_expr
                    .expr
                    .as_ref()
                    .map_or(Ir::Empty, |value| self.expr(value));
                if self.nested == 0 {
                    self.returns.push(value);
                }
            }
            _ => {}
        }
    }

    fn call(&mut self, call: &ExprCall) -> Ir {
        let arguments: Vec<Ir> = call.args.iter().map(|arg| self.expr(arg)).collect();
        let path = callee_path(&call.func);
        if path.is_none() {
            self.expr(&call.func);
        }
        let id = self.calls.len();
        self.bind_mut_references(call.args.iter(), id);
        let resolved = path.map(|path| self.file.resolve(path));
        let callee_name = path.and_then(|path| path.segments.last().map(|s| s.ident.to_string()));
        let sanitizer = resolved.as_deref().is_some_and(|resolved| {
            PATH_SANITIZERS.contains(&resolved)
                || resolved
                    .split_once("::")
                    .is_some_and(|(first, _)| CLEAN_TYPES.contains(&first))
        });
        let sink = resolved.as_deref().and_then(|resolved| {
            PATH_SINKS
                .iter()
                .find(|(rule, ..)| *rule == resolved)
                .map(|&(rule, kind, selector)| SinkSpec {
                    kind,
                    rule: rule.to_owned(),
                    arguments: selector.positions(arguments.len()),
                })
        });
        self.calls.push(CallDraft {
            line: call.span().start().line,
            callee_name,
            label: render_tokens(&call.func),
            receiver: Ir::Empty,
            arguments,
            method_syntax: false,
            sanitizer,
            sink,
        });
        Ir::Call(id)
    }

    fn method_call(&mut self, call: &ExprMethodCall) -> Ir {
        let receiver = self.expr(&call.receiver);
        let arguments: Vec<Ir> = call.args.iter().map(|arg| self.expr(arg)).collect();
        let id = self.calls.len();
        self.bind_mut_references(call.args.iter(), id);
        // A library method writes its inputs into a plain local receiver
        // (`buf.push_str(s)`, `cmd.arg(x)`).
        if let Expr::Path(path) = call.receiver.as_ref()
            && let Some(name) = single_ident(path)
            && !self.handles.contains(&name)
        {
            self.writes.push((name, Ir::Inputs(id)));
        }
        let name = call.method.to_string();
        let numeric_parse = name == "parse"
            && call.turbofish.as_ref().is_some_and(|turbofish| {
                turbofish
                    .args
                    .iter()
                    .all(|arg| matches!(arg, syn::GenericArgument::Type(ty) if is_clean_type(ty)))
            });
        let sink = METHOD_SINKS
            .iter()
            .find(|(method, .., floor)| *method == name && arguments.len() >= *floor)
            .map(|&(method, kind, selector, _)| SinkSpec {
                kind,
                rule: format!(".{method}"),
                arguments: selector.positions(arguments.len()),
            });
        self.calls.push(CallDraft {
            line: call.span().start().line,
            callee_name: Some(name.clone()),
            label: format!("{}.{name}", short(&render_tokens(&call.receiver))),
            receiver,
            arguments,
            method_syntax: true,
            sanitizer: numeric_parse || CLEAN_METHODS.contains(&name.as_str()),
            sink,
        });
        Ir::Call(id)
    }

    /// `&mut v` arguments receive whatever flows into a library call
    /// (`read_to_string(&mut buf)`).
    fn bind_mut_references<'e>(&mut self, args: impl Iterator<Item = &'e Expr>, id: usize) {
        for arg in args {
            if let Expr::Reference(reference) = arg
                && reference.mutability.is_some()
                && let Some(name) = self.write_target(&reference.expr)
            {
                self.writes.push((name, Ir::Inputs(id)));
            }
        }
    }

    /// The macro's arguments as expressions, plus the names its format
    /// string captures inline.
    fn mac(&mut self, mac: &Macro) -> Ir {
        let args = macro_argument_exprs(mac);
        let mut items: Vec<Ir> = args.iter().map(|arg| self.expr(arg)).collect();
        if let Some(Expr::Lit(literal)) = args.first()
            && let Lit::Str(text) = &literal.lit
        {
            for name in captured_names(&text.value()) {
                if !self.pinned.contains(&name) {
                    items.push(Ir::Var(name));
                }
            }
        }
        many(items)
    }

    fn assign(&mut self, target: &Expr, value: Ir) {
        if let Some(name) = self.write_target(target) {
            self.writes.push((name, value));
        }
    }

    /// The variable a write to `target` taints: the variable itself, or
    /// the base of a field, index or dereference write unless that base
    /// is a parameter or `self`.
    fn write_target(&self, target: &Expr) -> Option<String> {
        let base = target_base(target)?;
        let direct = matches!(target, Expr::Path(_));
        (direct || !self.handles.contains(&base)).then_some(base)
    }

    fn bind_pattern(&mut self, pat: &Pat, value: &Ir) {
        if let Pat::Type(pat_type) = pat
            && is_clean_type(&pat_type.ty)
        {
            return;
        }
        for name in pattern_names(pat) {
            self.writes.push((name, value.clone()));
        }
    }

    /// Names a condition pins to a constant when it holds.
    fn pinned_by(&self, condition: &Expr) -> Vec<String> {
        match condition {
            Expr::Paren(paren) => self.pinned_by(&paren.expr),
            Expr::Binary(binary) => match binary.op {
                BinOp::And(_) => {
                    let mut both = self.pinned_by(&binary.left);
                    both.extend(self.pinned_by(&binary.right));
                    both
                }
                BinOp::Or(_) => {
                    let right = self.pinned_by(&binary.right);
                    self.pinned_by(&binary.left)
                        .into_iter()
                        .filter(|name| right.contains(name))
                        .collect()
                }
                BinOp::Eq(_) => [(&binary.left, &binary.right), (&binary.right, &binary.left)]
                    .into_iter()
                    .find(|(_, value)| expr_is_constant(value))
                    .and_then(|(subject, _)| pin_subject(subject))
                    .into_iter()
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn propagate(&mut self) {
        loop {
            let mut changed = false;
            for index in 0..self.writes.len() {
                let origins = self.eval(&self.writes[index].1);
                let slot = self.env.entry(self.writes[index].0.clone()).or_default();
                if !origins.is_subset(slot) {
                    slot.extend(origins);
                    changed = true;
                }
            }
            if !changed {
                return;
            }
        }
    }

    fn eval(&self, ir: &Ir) -> BTreeSet<Origin> {
        match ir {
            Ir::Empty => BTreeSet::new(),
            Ir::Var(name) => self.env.get(name).cloned().unwrap_or_default(),
            Ir::Call(id) => BTreeSet::from([Origin::CallResult(*id)]),
            Ir::Inputs(id) => BTreeSet::from([Origin::CallInputs(*id)]),
            Ir::Many(items) => items.iter().flat_map(|item| self.eval(item)).collect(),
        }
    }

    fn flow_call(&self, draft: &CallDraft) -> FlowCall {
        FlowCall {
            line: draft.line,
            callee_name: draft.callee_name.clone(),
            callee_label: draft.label.clone(),
            receiver: self.eval(&draft.receiver),
            method_syntax: draft.method_syntax,
            arguments: draft.arguments.iter().map(|arg| self.eval(arg)).collect(),
            sanitizer: draft.sanitizer,
            sink: draft.sink.clone(),
        }
    }
}

/// The path a call's callee names, through `&` and parens. A qualified-self
/// path (`<S as T>::run`) is named by its last segment, as the call
/// index names it.
fn callee_path(expr: &Expr) -> Option<&syn::Path> {
    match expr {
        Expr::Path(path) => Some(&path.path),
        Expr::Reference(reference) => callee_path(&reference.expr),
        Expr::Paren(paren) => callee_path(&paren.expr),
        _ => None,
    }
}

/// A one-segment, lowercase path: a local, a parameter, or `self`.
fn single_ident(path: &syn::ExprPath) -> Option<String> {
    if path.qself.is_some() || path.path.leading_colon.is_some() || path.path.segments.len() != 1 {
        return None;
    }
    let name = path.path.segments.first()?.ident.to_string();
    (!lens_domain::starts_uppercase(&name)).then_some(name)
}

/// The variable a place expression writes into.
fn target_base(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) => single_ident(path),
        Expr::Field(field) => target_base(&field.base),
        Expr::Index(index) => target_base(&index.expr),
        // Only `*v` is a place; other unary operators cannot be written to.
        Expr::Unary(unary) => target_base(&unary.expr),
        Expr::Paren(paren) => target_base(&paren.expr),
        _ => None,
    }
}

/// The variable an allowlist check constrains: `x`, or the receiver of
/// a borrowing view of it (`x.as_str()`), through `&`, `*` and parens.
fn pin_subject(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) => single_ident(path),
        Expr::MethodCall(call)
            if call.args.is_empty()
                && matches!(
                    call.method.to_string().as_str(),
                    "as_str" | "as_ref" | "as_deref" | "as_bytes"
                ) =>
        {
            pin_subject(&call.receiver)
        }
        Expr::Reference(reference) => pin_subject(&reference.expr),
        Expr::Unary(unary) if matches!(unary.op, UnOp::Deref(_)) => pin_subject(&unary.expr),
        Expr::Paren(paren) => pin_subject(&paren.expr),
        _ => None,
    }
}

/// A literal, or a path Rust's naming convention reserves for constants
/// and variants (`MAX`, `Mode::Fast`).
fn expr_is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(_) => true,
        Expr::Path(path) => single_ident(path).is_none(),
        Expr::Reference(reference) => expr_is_constant(&reference.expr),
        Expr::Paren(paren) => expr_is_constant(&paren.expr),
        _ => false,
    }
}

/// An arm pattern that only matches constants: literals, paths to unit
/// variants or constants, and alternatives of those.
fn pattern_is_constant(pat: &Pat) -> bool {
    match pat {
        Pat::Lit(_) => true,
        Pat::Path(path) => expr_is_constant(&Expr::Path(path.clone())),
        Pat::Or(or) => or.cases.iter().all(pattern_is_constant),
        Pat::Reference(reference) => pattern_is_constant(&reference.pat),
        Pat::Paren(paren) => pattern_is_constant(&paren.pat),
        _ => false,
    }
}

/// Every name a pattern binds.
fn pattern_names(pat: &Pat) -> Vec<String> {
    let mut out = Vec::new();
    collect_pattern_names(pat, &mut out);
    out
}

fn collect_pattern_names(pat: &Pat, out: &mut Vec<String>) {
    match pat {
        Pat::Ident(ident) => {
            out.push(ident.ident.to_string());
            if let Some((_, sub)) = &ident.subpat {
                collect_pattern_names(sub, out);
            }
        }
        Pat::Reference(reference) => collect_pattern_names(&reference.pat, out),
        Pat::Paren(paren) => collect_pattern_names(&paren.pat, out),
        Pat::Type(pat_type) => collect_pattern_names(&pat_type.pat, out),
        Pat::Tuple(tuple) => tuple
            .elems
            .iter()
            .for_each(|p| collect_pattern_names(p, out)),
        Pat::TupleStruct(tuple) => tuple
            .elems
            .iter()
            .for_each(|p| collect_pattern_names(p, out)),
        Pat::Slice(slice) => slice
            .elems
            .iter()
            .for_each(|p| collect_pattern_names(p, out)),
        Pat::Or(or) => or.cases.iter().for_each(|p| collect_pattern_names(p, out)),
        Pat::Struct(pat_struct) => pat_struct
            .fields
            .iter()
            .for_each(|field| collect_pattern_names(&field.pat, out)),
        _ => {}
    }
}

/// Identifiers a format string captures inline: the `name` of `{name}`,
/// `{name:?}` and `{name:>8}`, but not positional `{0}` or escaped `{{`.
fn captured_names(format: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = format;
    while let Some(open) = rest.find('{') {
        rest = &rest[open + 1..];
        if let Some(escaped) = rest.strip_prefix('{') {
            rest = escaped;
            continue;
        }
        let end = rest.find(['}', ':']).unwrap_or(rest.len());
        let name = &rest[..end];
        if name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            out.push(name.to_owned());
        }
        rest = &rest[end..];
    }
    out
}

/// `render_tokens` output with the token spacing it leaves around
/// generics and tuple-struct patterns removed: `Query (p): Query <
/// Params >` reads `Query(p): Query<Params>`.
fn compact_tokens(text: &str) -> String {
    text.replace(" (", "(")
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
}

/// A receiver rendering short enough for a report label.
fn short(text: &str) -> String {
    const LIMIT: usize = 40;
    if text.chars().count() <= LIMIT {
        return text.to_owned();
    }
    let head: String = text.chars().take(LIMIT).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::trace_taint;
    use rstest::rstest;

    /// `(kind, rule)` of every finding in `body`, resolving calls to
    /// same-file functions by bare name — enough of a call graph for the
    /// adapter's half of the contract.
    fn findings(body: &str) -> Vec<(String, String)> {
        let source = format!(
            "use std::process::Command;\nuse std::fs;\n\
             use axum::extract::{{Query, Path}};\nuse axum::response::{{Html, Redirect}};\n\
             use actix_web::{{web, HttpRequest}};\n\n{body}\n"
        );
        let flows = extract_taint_flows(&source, &[]).unwrap();
        let file = syn::parse_file(&source).unwrap();
        let mut names = Vec::new();
        walk_fn_items(&file.items, WalkOptions::default(), &mut |site| {
            names.push(site.sig.ident.to_string());
        });
        let callees: Vec<Vec<Option<usize>>> = flows
            .iter()
            .map(|flow| {
                flow.calls
                    .iter()
                    .map(|call| {
                        call.callee_name
                            .as_ref()
                            .and_then(|name| names.iter().position(|n| n == name))
                    })
                    .collect()
            })
            .collect();
        trace_taint(&flows, &callees)
            .into_iter()
            .map(|finding| {
                let call = &flows[finding.sink.call.function].calls[finding.sink.call.call];
                let spec = call.sink.as_ref().unwrap();
                (spec.kind.to_owned(), spec.rule.clone())
            })
            .collect()
    }

    fn one(kind: &str, rule: &str) -> Vec<(String, String)> {
        vec![(kind.to_owned(), rule.to_owned())]
    }

    #[rstest]
    #[case::query_extractor_into_a_shell(
        "async fn h(Query(q): Query<Params>) {\n\
         \tCommand::new(&q.cmd).status().unwrap();\n}",
        one("command-injection", "std::process::Command::new")
    )]
    #[case::format_capture_into_sqlx(
        "async fn h(Path(name): Path<String>, pool: Pool) {\n\
         \tlet q = format!(\"SELECT * FROM users WHERE name = '{name}'\");\n\
         \tsqlx::query(&q).fetch_all(&pool).await;\n}",
        one("sql-injection", "sqlx::query")
    )]
    #[case::format_argument_into_rusqlite(
        "fn h(req: HttpRequest, conn: Connection) {\n\
         \tlet sql = format!(\"DELETE FROM t WHERE id = {}\", req.query_string());\n\
         \tconn.execute(&sql, []).unwrap();\n}",
        one("sql-injection", ".execute")
    )]
    #[case::bound_parameters_are_safe(
        "fn h(req: HttpRequest, conn: Connection) {\n\
         \tconn.execute(\"DELETE FROM t WHERE id = ?1\", [req.query_string()]).unwrap();\n}",
        vec![]
    )]
    #[case::sqlx_execute_on_a_query_is_not_a_sink(
        "async fn h(Path(name): Path<String>, pool: Pool) {\n\
         \tsqlx::query(\"SELECT 1 WHERE n = $1\").bind(name).execute(&pool).await;\n}",
        vec![]
    )]
    #[case::actix_path_through_a_helper(
        "fn load(name: &str) -> String { fs::read_to_string(name).unwrap() }\n\
         async fn h(p: web::Path<String>) -> String { load(&p.into_inner()) }",
        one("path-traversal", "std::fs::read_to_string")
    )]
    #[case::command_builder_arg(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet mut cmd = Command::new(\"git\");\n\
         \tcmd.arg(q.rev);\n\
         \tcmd.status();\n}",
        one("command-injection", ".arg")
    )]
    #[case::numeric_parse_sanitizes(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet n = q.n.parse::<u32>().unwrap();\n\
         \tCommand::new(format!(\"job-{n}\")).status();\n}",
        vec![]
    )]
    #[case::numeric_annotation_sanitizes(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet n: u32 = q.n.parse().unwrap();\n\
         \tCommand::new(format!(\"job-{n}\")).status();\n}",
        vec![]
    )]
    #[case::an_allowlist_on_a_field_does_not_pin(
        "fn h(Query(q): Query<Params>) {\n\
         \tmatch q.mode.as_str() {\n\
         \t\t\"fast\" | \"slow\" => { Command::new(q.mode.as_str()); }\n\
         \t\t_ => {}\n\t}\n}",
        one("command-injection", "std::process::Command::new")
    )]
    #[case::match_allowlist_on_a_local_pins(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet mode = q.mode;\n\
         \tmatch mode.as_str() {\n\
         \t\t\"fast\" | \"slow\" => { Command::new(mode); }\n\
         \t\tother => { Command::new(other); }\n\t}\n}",
        one("command-injection", "std::process::Command::new")
    )]
    #[case::if_allowlist_pins(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet mode = q.mode;\n\
         \tif mode == \"fast\" { Command::new(&mode); } else { fs::remove_file(&mode); }\n}",
        one("path-traversal", "std::fs::remove_file")
    )]
    #[case::html_response_and_redirect(
        "fn h(Query(q): Query<Params>) -> (Html<String>, Redirect) {\n\
         \t(Html(format!(\"<p>{}</p>\", q.name)), Redirect::to(&q.next))\n}",
        vec![
            ("open-redirect".to_owned(), "axum::response::Redirect::to".to_owned()),
            ("xss".to_owned(), "axum::response::Html".to_owned()),
        ]
    )]
    #[case::returned_through_match_tail(
        "fn pick(q: &Params) -> String { match q.kind { 1 => q.a.clone(), _ => q.b.clone() } }\n\
         fn h(Query(q): Query<Params>) { fs::write(pick(&q), \"x\"); }",
        one("path-traversal", "std::fs::write")
    )]
    #[case::self_method_by_path(
        "struct S;\nimpl S {\n\
         \tfn run(&self, cmd: String) { Command::new(cmd); }\n\
         \tfn h(&self, Query(q): Query<Params>) { Self::run(self, q.cmd); }\n}",
        one("command-injection", "std::process::Command::new")
    )]
    #[case::closure_capture(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet names: Vec<String> = vec![1].iter().map(|_| q.name.clone()).collect();\n\
         \tfor n in names { fs::remove_file(n); }\n}",
        one("path-traversal", "std::fs::remove_file")
    )]
    #[case::mut_reference_receives_inputs(
        "fn h(Query(q): Query<Params>) {\n\
         \tlet mut buf = String::new();\n\
         \tappend(&mut buf, &q.path);\n\
         \tfs::read(&buf);\n}",
        one("path-traversal", "std::fs::read")
    )]
    #[case::constants_are_clean(
        "fn h(Query(q): Query<Params>) { Command::new(\"ls\").arg(\"-la\").status(); }",
        vec![]
    )]
    #[case::no_source_no_finding("fn run(cmd: &str) { Command::new(cmd); }", vec![])]
    fn flows_from_rust_source(#[case] body: &str, #[case] expected: Vec<(String, String)>) {
        let mut found = findings(body);
        found.sort();
        assert_eq!(found, expected);
    }

    #[test]
    fn extra_source_types_extend_the_defaults() {
        let source = "use crate::rpc::RunRequest;\n\
                      fn run(req: RunRequest) { std::process::Command::new(req.cmd); }\n";
        assert!(
            extract_taint_flows(source, &[]).unwrap()[0]
                .sources
                .is_empty()
        );
        let with = extract_taint_flows(source, &["crate::rpc::RunRequest".to_owned()]).unwrap();
        assert_eq!(with[0].sources.len(), 1);
        assert_eq!(with[0].sources[0].label, "req: RunRequest");
        assert_eq!(with[0].sources[0].line, 2);
    }

    #[test]
    fn signature_slots_skip_self_and_clean_types() {
        let source = "struct S;\nimpl S {\n\
                      \tfn f(&self, n: u64, s: String, Wrap(t): Wrap) -> String { format!(\"{n}{s}{t}\") }\n}\n";
        let flow = &extract_taint_flows(source, &[]).unwrap()[0];
        assert!(flow.takes_receiver);
        assert_eq!(flow.start_line, 3);
        assert_eq!(
            flow.returns,
            BTreeSet::from([Origin::Param(1), Origin::Param(2)])
        );
    }

    #[test]
    fn source_labels_read_like_the_signature() {
        let source = "use axum::extract::Query;\nfn h(Query(p): Query<Params<u8>>) {}\n";
        let flow = &extract_taint_flows(source, &[]).unwrap()[0];
        assert_eq!(flow.sources[0].label, "Query(p): Query<Params<u8>>");
    }

    /// The single function in `source`, after an actix `HttpRequest`
    /// import.
    fn flow(source: &str) -> FunctionFlow {
        let source = format!("use actix_web::HttpRequest;\n{source}\n");
        let mut flows = extract_taint_flows(&source, &[]).unwrap();
        assert_eq!(flows.len(), 1, "{source}");
        flows.remove(0)
    }

    fn params(slots: &[usize]) -> BTreeSet<Origin> {
        slots.iter().map(|&slot| Origin::Param(slot)).collect()
    }

    /// The origins of each argument of every call to `g`.
    fn g_arguments(flow: &FunctionFlow) -> Vec<BTreeSet<Origin>> {
        flow.calls
            .iter()
            .filter(|call| call.callee_name.as_deref() == Some("g"))
            .flat_map(|call| call.arguments.clone())
            .collect()
    }

    /// A local built by the body's first call, then written `extra`.
    fn constructed_and(extra: &[Origin]) -> BTreeSet<Origin> {
        let mut origins = BTreeSet::from([Origin::CallResult(0)]);
        origins.extend(extra.iter().copied());
        origins
    }

    #[rstest]
    #[case::arithmetic_carries_both("g(x + &y)", vec![params(&[0, 1])])]
    #[case::a_comparison_is_a_bool("g(x == y)", vec![BTreeSet::new()])]
    #[case::not_is_a_bool("g(!x)", vec![BTreeSet::new()])]
    #[case::deref_carries("g(*x)", vec![params(&[0])])]
    #[case::numeric_cast_is_clean("g(x as u32)", vec![BTreeSet::new()])]
    #[case::other_cast_carries("g(x as Name)", vec![params(&[0])])]
    #[case::parens_carry("g((x))", vec![params(&[0])])]
    #[case::try_carries("g(x?)", vec![params(&[0])])]
    #[case::index_reads_the_base("g(x[0])", vec![params(&[0])])]
    #[case::array_literal("g([x])", vec![params(&[0])])]
    #[case::struct_literal("g(S { a: x, ..y })", vec![params(&[0, 1])])]
    #[case::repeat_literal("g([x; 3])", vec![params(&[0])])]
    #[case::range("g(x..y)", vec![params(&[0, 1])])]
    #[case::unsafe_block_tail("g(unsafe { x })", vec![params(&[0])])]
    #[case::async_block_tail("g(async { x })", vec![params(&[0])])]
    #[case::compound_assignment("let mut s = String::new();\n\ts += &x;\n\tg(s)", vec![constructed_and(&[Origin::Param(0)])])]
    #[case::assignment("let s;\n\ts = x;\n\tg(s)", vec![params(&[0])])]
    #[case::if_let_binds("if let Some(v) = x { g(v) }", vec![params(&[0])])]
    #[case::while_let_binds("while let Some(v) = x { g(v) }", vec![params(&[0])])]
    #[case::calls_inside_loop("loop { g(x); }", vec![params(&[0])])]
    #[case::calls_in_break_value("let v = loop { break g(x); };", vec![params(&[0])])]
    #[case::local_field_write_taints("let mut s = S::default();\n\ts.a = x;\n\tg(s)", vec![constructed_and(&[Origin::Param(0)])])]
    #[case::local_index_write_taints("let mut v = vec![];\n\tv[0] = x;\n\tg(v)", vec![params(&[0])])]
    #[case::parenthesised_write("let mut s = S::default();\n\t(s) = x;\n\tg(s)", vec![constructed_and(&[Origin::Param(0)])])]
    #[case::deref_write("let mut s = S::default();\n\tlet r = &mut s;\n\t*r = x;\n\tg(r)", vec![constructed_and(&[Origin::Param(0)])])]
    #[case::handle_field_write_does_not_taint("y.a = x;\n\tg(y)", vec![params(&[1])])]
    #[case::handle_reassignment_taints("y = x;\n\tg(y)", vec![params(&[0, 1])])]
    #[case::local_receiver_is_written("let mut b = String::new();\n\tb.push_str(&x);\n\tg(b)", vec![constructed_and(&[Origin::CallInputs(1)])])]
    #[case::handle_receiver_is_not_written("y.push_str(&x);\n\tg(y)", vec![params(&[1])])]
    #[case::reference_pattern("let &v = x;\n\tg(v)", vec![params(&[0])])]
    #[case::paren_pattern("let (v) = x;\n\tg(v)", vec![params(&[0])])]
    #[case::tuple_pattern("let (a, _b) = x;\n\tg(a)", vec![params(&[0])])]
    #[case::slice_pattern("let [a, ..] = x;\n\tg(a)", vec![params(&[0])])]
    #[case::or_pattern("if let Ok(v) | Err(v) = x { g(v) }", vec![params(&[0])])]
    #[case::struct_pattern("let S { a, .. } = x;\n\tg(a)", vec![params(&[0])])]
    #[case::qualified_paths_are_not_locals("g(x::CONST, <y>::z, ::x)", vec![BTreeSet::new(), BTreeSet::new(), BTreeSet::new()])]
    #[case::a_numeric_from_sanitizes("g(u32::from(x))", vec![BTreeSet::from([Origin::CallResult(0)])])]
    #[case::paren_eq_pins("if (x == \"a\") { g(x) }", vec![BTreeSet::new()])]
    #[case::deref_eq_pins("if *x == \"a\" { g(x) }", vec![BTreeSet::new()])]
    #[case::negation_does_not_pin("if -x == 1 { g(x) }", vec![params(&[0])])]
    #[case::conjunct_pins("if x == \"a\" && y.is_empty() { g(x) }", vec![BTreeSet::new()])]
    #[case::same_var_disjunction_pins("if x == \"a\" || x == \"b\" { g(x) }", vec![BTreeSet::new()])]
    #[case::mixed_disjunction_pins_nothing("if x == \"a\" || y == \"b\" { g(x) }", vec![params(&[0])])]
    #[case::variable_comparison_pins_nothing("if x == y { g(x) }", vec![params(&[0])])]
    #[case::uppercase_constant_pins("if x == MAX { g(x) }", vec![BTreeSet::new()])]
    #[case::variant_constant_pins("if x == Mode::Fast { g(x) }", vec![BTreeSet::new()])]
    #[case::reference_constant_pins("if x == &\"a\" { g(x) }", vec![BTreeSet::new()])]
    #[case::paren_constant_pins("if x == (\"a\") { g(x) }", vec![BTreeSet::new()])]
    #[case::match_paren_subject_pins("match (x) { \"a\" => g(x), _ => {} }", vec![BTreeSet::new()])]
    #[case::match_through_trim_does_not_pin("match x.trim() { \"a\" => g(x), _ => {} }", vec![params(&[0])])]
    #[case::match_through_to_string_does_not_pin("match x.to_string() { \"a\" => g(x), _ => {} }", vec![params(&[0])])]
    #[case::match_reference_subject_pins("match &x { \"a\" => g(x), _ => {} }", vec![BTreeSet::new()])]
    #[case::catch_all_arm_does_not_pin("match x.as_str() { \"a\" => {} _ => g(x) }", vec![params(&[0])])]
    #[case::variant_arm_pins("match x { Mode::Fast => g(x), _ => {} }", vec![BTreeSet::new()])]
    #[case::reference_arm_pins("match x { &\"a\" => g(x), _ => {} }", vec![BTreeSet::new()])]
    #[case::paren_arm_pins("match x { (\"a\") => g(x), _ => {} }", vec![BTreeSet::new()])]
    #[case::paren_callee("(g)(x)", vec![params(&[0])])]
    #[case::reference_callee("(&g)(y)", vec![params(&[1])])]
    fn lowering(#[case] body: &str, #[case] expected: Vec<BTreeSet<Origin>>) {
        let flow = flow(&format!("fn f(x: String, y: String) {{\n\t{body}\n}}"));
        assert_eq!(g_arguments(&flow), expected, "{body}");
    }

    #[test]
    fn a_numeric_from_is_a_sanitizer_call() {
        let flow = flow("fn f(x: String) { g(u32::from(x), ammonia::clean(&x), String::from(x)) }");
        let sanitizers: Vec<bool> = flow.calls.iter().map(|call| call.sanitizer).collect();
        assert_eq!(sanitizers, [true, true, false, false]);
    }

    #[rstest]
    #[case::numeric_turbofish("x.parse::<u32>()", true)]
    #[case::other_turbofish("x.parse::<Url>()", false)]
    #[case::numeric_turbofish_on_another_method("x.get::<u8>()", false)]
    #[case::clean_method("x.len()", true)]
    #[case::plain_method("x.trim()", false)]
    fn method_sanitizers(#[case] call: &str, #[case] sanitizer: bool) {
        let flow = flow(&format!("fn f(x: String) {{ {call}; }}"));
        assert_eq!(flow.calls[0].sanitizer, sanitizer);
    }

    #[test]
    fn returns_count_the_tail_and_explicit_returns_but_not_closures_or_nested_fns() {
        let flow = flow(
            "fn f(x: String, y: String, z: String) -> String {\n\
             \tlet c = || { return y; };\n\
             \tfn inner(z: String) -> String { return z; }\n\
             \tif x.is_empty() { return x; }\n\
             \tg(c);\n\
             \tString::new()\n}",
        );
        assert_eq!(
            flow.returns,
            BTreeSet::from([Origin::Param(0), Origin::CallResult(2)])
        );
    }

    #[test]
    fn nested_fns_and_impls_contribute_their_calls() {
        let flow = flow(
            "fn f() {\n\
             \tfn inner() { g(1); }\n\
             \tstruct S;\n\
             \timpl S { fn m(&self) { g(2); } }\n\
             \tg(3);\n}",
        );
        assert_eq!(g_arguments(&flow).len(), 3);
    }

    #[test]
    fn each_source_gets_its_own_index() {
        let flow = flow("fn h(a: HttpRequest, b: &HttpRequest) { g(b) }");
        assert_eq!(flow.sources.len(), 2);
        assert_eq!(g_arguments(&flow), [BTreeSet::from([Origin::Source(1)])]);
    }

    #[test]
    fn long_receivers_are_shortened_in_labels() {
        let flow = flow(
            "fn f(x: String) { some_long_builder_name.with_a_long_method_chain().and_more(x).finish(); }",
        );
        let label = &flow.calls[2].callee_label;
        assert!(label.ends_with("….finish"), "{label}");
        assert_eq!(label.chars().count(), 40 + "….finish".chars().count());
        assert_eq!(
            flow.calls[0].callee_label,
            "some_long_builder_name.with_a_long_method_chain"
        );
    }

    #[rstest]
    #[case("SELECT {name} FROM {table:?}", vec!["name", "table"])]
    #[case("{{literal}} {0} {} {x:>8}", vec!["x"])]
    #[case("{9lives}", vec![])]
    #[case("{_private}", vec!["_private"])]
    fn format_captures(#[case] format: &str, #[case] expected: Vec<&str>) {
        assert_eq!(captured_names(format), expected);
    }
}

//! Per-function taint flow extraction for `analyze taint`.
//!
//! Lowers every top-level Go function into a [`FunctionFlow`]: which
//! untrusted values it introduces, where each call's receiver and
//! arguments may have come from, and where its return values may have
//! come from. The lowering is syntax-only and flow-insensitive — every
//! assignment in the body (closures included) feeds one name → origins
//! map, iterated to a fixpoint — so a variable is tainted if *any* write
//! to it is. Assignments through a field, an index or a dereference
//! taint the base variable (`req.Name = x` taints `req`).
//!
//! Sources are parameters whose declared type is a request handle of a
//! known HTTP framework ([`DEFAULT_SOURCE_TYPES`]) plus whatever the
//! caller adds. Sinks and sanitizers are matched on the import path the
//! call's package qualifier resolves to (`exec.Command` is
//! `os/exec.Command` only when `exec` imports `os/exec`), or, for the
//! `database/sql`-shaped methods whose receiver type a syntax-only pass
//! cannot know, on the method name.

use std::collections::{BTreeSet, HashMap, HashSet};

use lens_domain::{FlowCall, FunctionFlow, Origin, SinkSpec, TaintSource, starts_uppercase};
use tree_sitter::Node;

use crate::call_index::{ImportAlias, import_specs};
use crate::node_text::node_str;
use crate::parser::{GoParseError, parameter_slot_names, parse_tree};
use crate::walk::walk_top_level_fns;

/// Parameter types treated as untrusted input, as `import/path.Type`. A
/// pointer to one counts too.
pub const DEFAULT_SOURCE_TYPES: &[&str] = &[
    "net/http.Request",
    "github.com/gin-gonic/gin.Context",
    "github.com/labstack/echo.Context",
    "github.com/labstack/echo/v4.Context",
    "github.com/gofiber/fiber/v2.Ctx",
    "github.com/gofiber/fiber/v3.Ctx",
    "github.com/valyala/fasthttp.RequestCtx",
];

/// Which arguments of a sink call are checked.
#[derive(Debug, Clone, Copy)]
enum Checked {
    All,
    At(usize),
    From(usize),
}

impl Checked {
    fn positions(self, argument_count: usize) -> Vec<usize> {
        match self {
            Self::All => (0..argument_count).collect(),
            Self::At(position) => (position < argument_count)
                .then_some(position)
                .into_iter()
                .collect(),
            Self::From(start) => (start..argument_count).collect(),
        }
    }
}

/// Package-level sinks: `(import path.Func, kind, checked arguments)`.
const PACKAGE_SINKS: &[(&str, &str, Checked)] = &[
    ("os/exec.Command", "command-injection", Checked::All),
    (
        "os/exec.CommandContext",
        "command-injection",
        Checked::From(1),
    ),
    ("syscall.Exec", "command-injection", Checked::All),
    ("os.StartProcess", "command-injection", Checked::All),
    ("os.Open", "path-traversal", Checked::At(0)),
    ("os.OpenFile", "path-traversal", Checked::At(0)),
    ("os.Create", "path-traversal", Checked::At(0)),
    ("os.ReadFile", "path-traversal", Checked::At(0)),
    ("os.WriteFile", "path-traversal", Checked::At(0)),
    ("os.ReadDir", "path-traversal", Checked::At(0)),
    ("os.Remove", "path-traversal", Checked::At(0)),
    ("os.RemoveAll", "path-traversal", Checked::At(0)),
    ("os.Mkdir", "path-traversal", Checked::At(0)),
    ("os.MkdirAll", "path-traversal", Checked::At(0)),
    ("os.Rename", "path-traversal", Checked::All),
    ("io/ioutil.ReadFile", "path-traversal", Checked::At(0)),
    ("io/ioutil.WriteFile", "path-traversal", Checked::At(0)),
    ("io/ioutil.ReadDir", "path-traversal", Checked::At(0)),
    ("net/http.ServeFile", "path-traversal", Checked::At(2)),
    ("net/http.Get", "ssrf", Checked::At(0)),
    ("net/http.Head", "ssrf", Checked::At(0)),
    ("net/http.Post", "ssrf", Checked::At(0)),
    ("net/http.PostForm", "ssrf", Checked::At(0)),
    ("net/http.NewRequest", "ssrf", Checked::At(1)),
    ("net/http.NewRequestWithContext", "ssrf", Checked::At(2)),
    ("net.Dial", "ssrf", Checked::At(1)),
    ("net.DialTimeout", "ssrf", Checked::At(1)),
    ("net/http.Redirect", "open-redirect", Checked::At(2)),
    ("html/template.HTML", "xss", Checked::At(0)),
    ("html/template.HTMLAttr", "xss", Checked::At(0)),
    ("html/template.JS", "xss", Checked::At(0)),
    ("html/template.JSStr", "xss", Checked::At(0)),
    ("html/template.CSS", "xss", Checked::At(0)),
    ("html/template.URL", "xss", Checked::At(0)),
    ("html/template.Srcset", "xss", Checked::At(0)),
];

/// Method sinks matched by name on calls the call graph leaves
/// unresolved: the `database/sql` query surface (shared by `sqlx`,
/// `pgx`'s stdlib adapter and most wrappers) and GORM's raw SQL. Only
/// the query-text argument is checked — bound parameters are the safe
/// path.
const METHOD_SINKS: &[(&str, &str, Checked)] = &[
    ("Query", "sql-injection", Checked::At(0)),
    ("QueryRow", "sql-injection", Checked::At(0)),
    ("Exec", "sql-injection", Checked::At(0)),
    ("Prepare", "sql-injection", Checked::At(0)),
    ("QueryContext", "sql-injection", Checked::At(1)),
    ("QueryRowContext", "sql-injection", Checked::At(1)),
    ("ExecContext", "sql-injection", Checked::At(1)),
    ("PrepareContext", "sql-injection", Checked::At(1)),
    ("Raw", "sql-injection", Checked::At(0)),
];

/// Methods returning a prepared statement. A [`METHOD_SINKS`] call on a
/// variable bound from one of these takes bind parameters only — the
/// query text was fixed at preparation — so it is not a sink.
const PREPARE_METHODS: &[&str] = &[
    "Prepare",
    "PrepareContext",
    "Preparex",
    "PreparexContext",
    "PrepareNamed",
    "PrepareNamedContext",
];

/// Package functions whose result carries none of their input's taint:
/// numeric parsing and escaping.
const PACKAGE_SANITIZERS: &[&str] = &[
    "strconv.Atoi",
    "strconv.ParseInt",
    "strconv.ParseUint",
    "strconv.ParseFloat",
    "strconv.ParseBool",
    "net/url.QueryEscape",
    "net/url.PathEscape",
    "html.EscapeString",
    "html/template.HTMLEscapeString",
    "html/template.JSEscapeString",
    "html/template.HTMLEscaper",
    "html/template.URLQueryEscaper",
    "text/template.HTMLEscapeString",
    "text/template.JSEscapeString",
];

/// Builtins and conversions whose result is a number or a boolean.
const CLEAN_BUILTINS: &[&str] = &[
    "len",
    "cap",
    "bool",
    "int",
    "int8",
    "int16",
    "int32",
    "int64",
    "uint",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "uintptr",
    "float32",
    "float64",
    "complex64",
    "complex128",
    "byte",
    "rune",
];

/// Lower every top-level function of `source` into a [`FunctionFlow`].
/// `extra_source_types` extends [`DEFAULT_SOURCE_TYPES`] with more
/// `import/path.Type` names.
pub fn extract_taint_flows(
    source: &str,
    extra_source_types: &[String],
) -> Result<Vec<FunctionFlow>, GoParseError> {
    let tree = parse_tree(source)?;
    let bytes = source.as_bytes();
    let packages = package_aliases(tree.root_node(), bytes);
    let source_types: Vec<&str> = DEFAULT_SOURCE_TYPES
        .iter()
        .copied()
        .chain(extra_source_types.iter().map(String::as_str))
        .collect();
    let file = FileContext {
        source: bytes,
        packages: &packages,
        source_types: &source_types,
    };
    let mut out = Vec::new();
    walk_top_level_fns(tree.root_node(), bytes, &mut |site| {
        out.push(FunctionLowering::lower(&file, site.node, site.body));
    });
    Ok(out)
}

/// Local package qualifier → import path. A path without an explicit
/// name is qualified by its package name, which by convention is the
/// last path segment with a major-version suffix dropped
/// (`github.com/labstack/echo/v4` → `echo`, `gopkg.in/yaml.v3` → `yaml`).
fn package_aliases(root: Node<'_>, source: &[u8]) -> HashMap<String, String> {
    import_specs(root, source)
        .into_iter()
        .filter_map(|spec| {
            let alias = match spec.alias {
                ImportAlias::Named(name) => name,
                ImportAlias::Hidden => return None,
                ImportAlias::Default => default_package_name(&spec.path)?,
            };
            Some((alias, spec.path))
        })
        .collect()
}

fn default_package_name(path: &str) -> Option<String> {
    let mut segments = path.rsplit('/').filter(|segment| !segment.is_empty());
    let last = segments.next()?;
    let name = if is_major_version(last) {
        segments.next()?
    } else {
        last
    };
    let name = match name.rsplit_once('.') {
        Some((stem, suffix)) if is_major_version(suffix) => stem,
        _ => name,
    };
    Some(name.to_owned())
}

fn is_major_version(segment: &str) -> bool {
    segment
        .strip_prefix('v')
        .is_some_and(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

struct FileContext<'a> {
    source: &'a [u8],
    packages: &'a HashMap<String, String>,
    source_types: &'a [&'a str],
}

/// What one assignment-shaped construct writes: `target` gets the
/// origins of every node in `values`, plus `extra`.
struct Binding<'tree> {
    target: String,
    values: Vec<Node<'tree>>,
    extra: BTreeSet<Origin>,
    /// Names pinned to a constant where the write happens — see
    /// [`FunctionLowering::pinned_at`].
    pinned: HashSet<String>,
}

struct FunctionLowering<'a, 'tree> {
    file: &'a FileContext<'a>,
    env: HashMap<String, BTreeSet<Origin>>,
    bindings: Vec<Binding<'tree>>,
    sources: Vec<TaintSource>,
    /// Call expressions in pre-order; the index is the call's id.
    calls: Vec<Node<'tree>>,
    call_ids: HashMap<usize, usize>,
    returns: Vec<Node<'tree>>,
    named_results: Vec<String>,
    /// Variables bound from a [`PREPARE_METHODS`] call.
    prepared: HashSet<String>,
    /// The declaration's own parameter and receiver names. A write into
    /// a field of one of these does not taint it: those are the
    /// long-lived handles (`ctx`, `s`, `w`) a handler stores
    /// request-derived values into, and tainting the whole handle would
    /// taint every value later read from it.
    handles: HashSet<String>,
}

impl<'a, 'tree> FunctionLowering<'a, 'tree> {
    fn lower(file: &'a FileContext<'a>, decl: Node<'tree>, body: Node<'tree>) -> FunctionFlow {
        let mut lowering = Self {
            file,
            env: HashMap::new(),
            bindings: Vec::new(),
            sources: Vec::new(),
            calls: Vec::new(),
            call_ids: HashMap::new(),
            returns: Vec::new(),
            named_results: Vec::new(),
            prepared: HashSet::new(),
            handles: HashSet::new(),
        };
        for field in ["receiver", "parameters"] {
            if let Some(params) = decl.child_by_field_name(field) {
                lowering.handles.extend(
                    parameter_slot_names(params, file.source)
                        .into_iter()
                        .flatten(),
                );
            }
        }
        if let Some(params) = decl.child_by_field_name("parameters") {
            lowering.bind_parameters(params, true);
        }
        if let Some(result) = decl.child_by_field_name("result")
            && result.kind() == "parameter_list"
        {
            lowering.named_results = parameter_slot_names(result, file.source)
                .into_iter()
                .flatten()
                .collect();
        }
        lowering.collect(body, false);
        lowering.propagate();
        lowering.finish(decl)
    }

    /// Bind each parameter to its slot, or to a fresh source when its
    /// type is a request handle. A closure's parameters have no slot in
    /// the enclosing function's summary, so only source-typed ones bind.
    fn bind_parameters(&mut self, params: Node<'tree>, slots: bool) {
        let mut slot = 0;
        let mut cursor = params.walk();
        for decl in params.named_children(&mut cursor) {
            if !matches!(
                decl.kind(),
                "parameter_declaration" | "variadic_parameter_declaration"
            ) {
                continue;
            }
            let names = crate::parser::declaration_names(decl, self.file.source);
            let written = decl
                .child_by_field_name("type")
                .and_then(|ty| node_str(ty, self.file.source));
            let source_type = written.filter(|ty| self.is_source_type(ty));
            let clean = written.is_some_and(|ty| self.is_clean_type(ty));
            // Go names every parameter or none, so an unnamed declaration
            // means nothing in this list can be read by name.
            for name in names {
                let origin = match source_type {
                    Some(ty) => {
                        self.sources.push(TaintSource {
                            label: format!("{name} {ty}"),
                            line: decl.start_position().row + 1,
                        });
                        Some(Origin::Source(self.sources.len() - 1))
                    }
                    None => (slots && !clean).then_some(Origin::Param(slot)),
                };
                if let Some(origin) = origin {
                    self.env.entry(name).or_default().insert(origin);
                }
                slot += 1;
            }
        }
    }

    /// A parameter of this type cannot carry an injection payload: a
    /// number or a boolean, or a `context.Context` — request-scoped
    /// values ride in one, but it is the handle every call is passed,
    /// and following it would taint everything downstream of a handler.
    fn is_clean_type(&self, written: &str) -> bool {
        CLEAN_BUILTINS.contains(&written)
            || written.split_once('.').is_some_and(|(qualifier, name)| {
                name == "Context"
                    && self.file.packages.get(qualifier).map(String::as_str) == Some("context")
            })
    }

    fn is_source_type(&self, written: &str) -> bool {
        let bare = written.trim_start_matches('*');
        let Some((qualifier, name)) = bare.split_once('.') else {
            return false;
        };
        let Some(path) = self.file.packages.get(qualifier) else {
            return false;
        };
        self.file
            .source_types
            .iter()
            .any(|ty| ty.rsplit_once('.') == Some((path.as_str(), name)))
    }

    /// One pre-order pass: number the calls, record every write, and
    /// collect the function's own `return` statements.
    fn collect(&mut self, node: Node<'tree>, in_closure: bool) {
        match node.kind() {
            "call_expression" => {
                self.call_ids.insert(node.id(), self.calls.len());
                self.calls.push(node);
                self.bind_call_writes(node);
            }
            "func_literal" => {
                if let Some(params) = node.child_by_field_name("parameters") {
                    self.bind_parameters(params, false);
                }
                if let Some(body) = node.child_by_field_name("body") {
                    self.collect(body, true);
                }
                return;
            }
            "short_var_declaration"
            | "assignment_statement"
            | "range_clause"
            | "receive_statement" => self.bind_lists(node, "left", "right"),
            "var_spec" => self.bind_var_spec(node),
            "type_switch_statement" => self.bind_lists(node, "alias", "value"),
            "return_statement" if !in_closure => {
                let mut cursor = node.walk();
                self.returns.extend(node.named_children(&mut cursor));
            }
            _ => {}
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.collect(child, in_closure);
        }
    }

    /// `a, b := x, y` pairs by position; `a, b := f()` (and a `range`,
    /// whose key and value both come from the ranged expression) gives
    /// every target the whole right-hand side.
    fn bind_lists(&mut self, node: Node<'tree>, left: &str, right: &str) {
        let (Some(left), Some(right)) = (
            node.child_by_field_name(left),
            node.child_by_field_name(right),
        ) else {
            return;
        };
        let targets = list_elements(left);
        let values = list_elements(right);
        let paired = targets.len() == values.len();
        for (position, target) in targets.into_iter().enumerate() {
            let Some(name) = self.write_target(target) else {
                continue;
            };
            let values = if paired {
                vec![values[position]]
            } else {
                values.clone()
            };
            self.bind(name, values);
        }
    }

    fn bind_var_spec(&mut self, node: Node<'tree>) {
        let Some(value) = node.child_by_field_name("value") else {
            return;
        };
        let values = list_elements(value);
        let mut cursor = node.walk();
        let names: Vec<String> = node
            .children_by_field_name("name", &mut cursor)
            .filter_map(|name| node_str(name, self.file.source))
            .map(str::to_owned)
            .collect();
        let paired = names.len() == values.len();
        for (position, name) in names.into_iter().enumerate() {
            let values = if paired {
                vec![values[position]]
            } else {
                values.clone()
            };
            self.bind(name, values);
        }
    }

    fn bind(&mut self, target: String, values: Vec<Node<'tree>>) {
        if values.iter().any(|&value| self.is_prepare_call(value)) {
            self.prepared.insert(target.clone());
        }
        let pinned = values
            .first()
            .map(|&value| self.pinned_at(value))
            .unwrap_or_default();
        self.bindings.push(Binding {
            target,
            values,
            extra: BTreeSet::new(),
            pinned,
        });
    }

    /// Only a `call_expression` has a `function` field.
    fn is_prepare_call(&self, node: Node<'_>) -> bool {
        node.child_by_field_name("function")
            .filter(|function| function.kind() == "selector_expression")
            .and_then(|function| function.child_by_field_name("field"))
            .and_then(|field| node_str(field, self.file.source))
            .is_some_and(|name| PREPARE_METHODS.contains(&name))
    }

    /// A library call writes its inputs through `&v` arguments
    /// (`Decode(&v)`, `Scan(&id)`) and into a plain-variable receiver
    /// (`b.WriteString(s)`, `q.Set(k, v)`).
    fn bind_call_writes(&mut self, call: Node<'tree>) {
        let id = self.calls.len() - 1;
        let mut targets = Vec::new();
        if let Some(args) = call.child_by_field_name("arguments") {
            let mut cursor = args.walk();
            for arg in args.named_children(&mut cursor) {
                if arg.kind() == "unary_expression"
                    && arg
                        .child_by_field_name("operator")
                        .is_some_and(|op| op.kind() == "&")
                    && let Some(operand) = arg.child_by_field_name("operand")
                    && let Some(name) = self.write_target(operand)
                {
                    targets.push(name);
                }
            }
        }
        if let Some(function) = call.child_by_field_name("function")
            && function.kind() == "selector_expression"
            && let Some(operand) = function.child_by_field_name("operand")
            && operand.kind() == "identifier"
            && let Some(name) = node_str(operand, self.file.source)
            && !self.is_package(name)
            && !self.handles.contains(name)
        {
            targets.push(name.to_owned());
        }
        for target in targets {
            self.bindings.push(Binding {
                target,
                values: Vec::new(),
                extra: BTreeSet::from([Origin::CallInputs(id)]),
                pinned: HashSet::new(),
            });
        }
    }

    /// The variable a write to `target` taints: the variable itself, or
    /// the base of a field, index or dereference write unless that base is
    /// one of the declaration's [`Self::handles`].
    fn write_target(&self, target: Node<'_>) -> Option<String> {
        let name = target_base(target, self.file.source)?;
        (target.kind() == "identifier" || !self.handles.contains(&name)).then_some(name)
    }

    /// A qualifier names an import unless a local of the same name
    /// shadows it.
    fn is_package(&self, name: &str) -> bool {
        self.file.packages.contains_key(name) && !self.env.contains_key(name)
    }

    fn propagate(&mut self) {
        loop {
            let mut changed = false;
            for index in 0..self.bindings.len() {
                let binding = &self.bindings[index];
                let mut origins = binding.extra.clone();
                for value in &binding.values {
                    origins.extend(self.origins(*value, &binding.pinned));
                }
                let target = binding.target.clone();
                let slot = self.env.entry(target).or_default();
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

    /// Names whose value is pinned to a constant wherever `node`
    /// evaluates: the `x` of an enclosing `if x == "a" {` (or `x == "a"
    /// || x == "b"`, or one conjunct of an `&&`), and the subject of an
    /// enclosing `switch x { case "a", "b": }` whose case values are all
    /// constants. An allowlist check is the commonest validation in Go
    /// handlers, and an allowlisted value carries no attacker choice.
    fn pinned_at(&self, node: Node<'_>) -> HashSet<String> {
        let mut pinned = HashSet::new();
        let mut child = node;
        while let Some(parent) = child.parent() {
            // A closure written inside the guarded branch captures the
            // guarded variable, so the pin reaches into it.
            match parent.kind() {
                "if_statement"
                    if parent
                        .child_by_field_name("consequence")
                        .is_some_and(|consequence| consequence.id() == child.id()) =>
                {
                    if let Some(condition) = parent.child_by_field_name("condition") {
                        pinned.extend(self.pinned_by(condition));
                    }
                }
                "expression_case" => {
                    if let Some(values) = parent.child_by_field_name("value")
                        && values.id() != child.id()
                        && list_elements(values)
                            .iter()
                            .all(|&value| self.is_constant(value))
                        && let Some(switch) = parent.parent()
                        && let Some(subject) = switch.child_by_field_name("value")
                        && subject.kind() == "identifier"
                        && let Some(name) = node_str(subject, self.file.source)
                    {
                        pinned.insert(name.to_owned());
                    }
                }
                _ => {}
            }
            child = parent;
        }
        pinned
    }

    /// Names a condition pins to a constant when it holds.
    fn pinned_by(&self, condition: Node<'_>) -> HashSet<String> {
        match condition.kind() {
            "parenthesized_expression" => {
                let mut cursor = condition.walk();
                let inner = condition.named_children(&mut cursor).next();
                inner.map(|inner| self.pinned_by(inner)).unwrap_or_default()
            }
            "binary_expression" => {
                let (Some(left), Some(right), Some(operator)) = (
                    condition.child_by_field_name("left"),
                    condition.child_by_field_name("right"),
                    condition.child_by_field_name("operator"),
                ) else {
                    return HashSet::new();
                };
                match operator.kind() {
                    "&&" => {
                        let mut both = self.pinned_by(left);
                        both.extend(self.pinned_by(right));
                        both
                    }
                    "||" => {
                        let right = self.pinned_by(right);
                        self.pinned_by(left)
                            .into_iter()
                            .filter(|name| right.contains(name))
                            .collect()
                    }
                    "==" => [(left, right), (right, left)]
                        .into_iter()
                        .find(|(name, value)| {
                            name.kind() == "identifier" && self.is_constant(*value)
                        })
                        .and_then(|(name, _)| node_str(name, self.file.source))
                        .map(|name| HashSet::from([name.to_owned()]))
                        .unwrap_or_default(),
                    _ => HashSet::new(),
                }
            }
            _ => HashSet::new(),
        }
    }

    /// A literal, or a name Go's export convention reserves for
    /// constants and types (`MaxSize`, `api.GithubService`).
    fn is_constant(&self, node: Node<'_>) -> bool {
        match node.kind() {
            "interpreted_string_literal"
            | "raw_string_literal"
            | "int_literal"
            | "float_literal"
            | "rune_literal"
            | "true"
            | "false"
            | "nil" => true,
            "identifier" => node_str(node, self.file.source).is_some_and(starts_uppercase),
            "selector_expression" => node
                .child_by_field_name("operand")
                .filter(|operand| operand.kind() == "identifier")
                .and_then(|operand| node_str(operand, self.file.source))
                .is_some_and(|qualifier| self.is_package(qualifier)),
            _ => false,
        }
    }

    /// Where the value of `node` may have come from, under the current
    /// name → origins map, with `pinned` names contributing nothing.
    fn origins(&self, node: Node<'_>, pinned: &HashSet<String>) -> BTreeSet<Origin> {
        match node.kind() {
            "identifier" => node_str(node, self.file.source)
                .filter(|name| !pinned.contains(*name))
                .and_then(|name| self.env.get(name))
                .cloned()
                .unwrap_or_default(),
            "call_expression" => self
                .call_ids
                .get(&node.id())
                .map(|&id| BTreeSet::from([Origin::CallResult(id)]))
                .unwrap_or_default(),
            // A closure's body is its own; what it captures flows only
            // through the calls and writes inside it. Everything else
            // (field selectors included: a `field_identifier` reads no
            // variable) carries what its parts carry.
            "func_literal" => BTreeSet::new(),
            _ => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .flat_map(|child| self.origins(child, pinned))
                    .collect()
            }
        }
    }

    fn finish(self, decl: Node<'_>) -> FunctionFlow {
        let calls = self
            .calls
            .iter()
            .map(|&call| self.flow_call(call))
            .collect();
        let mut returns: BTreeSet<Origin> = self
            .returns
            .iter()
            .flat_map(|&node| self.origins(node, &self.pinned_at(node)))
            .collect();
        for name in &self.named_results {
            if let Some(origins) = self.env.get(name) {
                returns.extend(origins.iter().copied());
            }
        }
        FunctionFlow {
            start_line: decl.start_position().row + 1,
            sources: self.sources,
            calls,
            returns,
        }
    }

    fn flow_call(&self, call: Node<'_>) -> FlowCall {
        let pinned = self.pinned_at(call);
        let arguments: Vec<BTreeSet<Origin>> = call
            .child_by_field_name("arguments")
            .map(|args| {
                let mut cursor = args.walk();
                args.named_children(&mut cursor)
                    .map(|arg| self.origins(arg, &pinned))
                    .collect()
            })
            .unwrap_or_default();
        let callee = call
            .child_by_field_name("function")
            .map(|function| self.callee(function))
            .unwrap_or(Callee::Opaque);
        let line = call.start_position().row + 1;
        let label = call
            .child_by_field_name("function")
            .and_then(|function| node_str(function, self.file.source))
            .unwrap_or("?")
            .to_owned();
        let mut flow = FlowCall {
            line,
            callee_name: None,
            callee_label: label,
            receiver: BTreeSet::new(),
            arguments,
            sanitizer: false,
            sink: None,
        };
        let count = flow.arguments.len();
        match callee {
            Callee::Name(name) => {
                flow.sanitizer = CLEAN_BUILTINS.contains(&name.as_str());
                flow.callee_name = Some(name);
            }
            Callee::Package { path, name } => {
                let qualified = format!("{path}.{name}");
                flow.sanitizer = PACKAGE_SANITIZERS.contains(&qualified.as_str());
                flow.sink = PACKAGE_SINKS
                    .iter()
                    .find(|(rule, ..)| *rule == qualified)
                    .map(|&(_, kind, checked)| SinkSpec {
                        kind,
                        rule: qualified.clone(),
                        arguments: checked.positions(count),
                    });
                flow.callee_name = Some(name);
            }
            Callee::Method { receiver, name } => {
                flow.receiver = self.origins(receiver, &pinned);
                let prepared = receiver.kind() == "identifier"
                    && node_str(receiver, self.file.source)
                        .is_some_and(|name| self.prepared.contains(name));
                flow.sink = METHOD_SINKS
                    .iter()
                    .filter(|_| !prepared)
                    .find(|(method, ..)| *method == name)
                    .map(|&(method, kind, checked)| SinkSpec {
                        kind,
                        rule: format!(".{method}"),
                        arguments: checked.positions(count),
                    });
                flow.callee_name = Some(name);
            }
            Callee::Opaque => {}
        }
        flow
    }

    fn callee<'n>(&self, function: Node<'n>) -> Callee<'n> {
        match function.kind() {
            "identifier" => node_str(function, self.file.source)
                .map_or(Callee::Opaque, |name| Callee::Name(name.to_owned())),
            "selector_expression" => {
                let (Some(operand), Some(field)) = (
                    function.child_by_field_name("operand"),
                    function.child_by_field_name("field"),
                ) else {
                    return Callee::Opaque;
                };
                let Some(name) = node_str(field, self.file.source).map(str::to_owned) else {
                    return Callee::Opaque;
                };
                let package = (operand.kind() == "identifier")
                    .then(|| node_str(operand, self.file.source))
                    .flatten()
                    .filter(|qualifier| self.is_package(qualifier))
                    .and_then(|qualifier| self.file.packages.get(qualifier));
                match package {
                    Some(path) => Callee::Package {
                        path: path.clone(),
                        name,
                    },
                    None => Callee::Method {
                        receiver: operand,
                        name,
                    },
                }
            }
            // `F[T](x)` names `F`; `(f)(x)` names `f`.
            "index_expression" | "parenthesized_expression" => {
                let inner = function.child_by_field_name("operand").or_else(|| {
                    let mut cursor = function.walk();
                    function.named_children(&mut cursor).next()
                });
                inner.map_or(Callee::Opaque, |inner| self.callee(inner))
            }
            _ => Callee::Opaque,
        }
    }
}

enum Callee<'n> {
    Name(String),
    Package { path: String, name: String },
    Method { receiver: Node<'n>, name: String },
    Opaque,
}

/// Flatten an `expression_list` into its elements; a bare node is a
/// one-element list.
fn list_elements(node: Node<'_>) -> Vec<Node<'_>> {
    if node.kind() == "expression_list" {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).collect()
    } else {
        vec![node]
    }
}

/// The variable an assignment target writes into: `v`, the `v` of
/// `v.f`, `v[i]` and `*v`. `_` writes nothing.
fn target_base(node: Node<'_>, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => node_str(node, source)
            .filter(|name| *name != "_")
            .map(str::to_owned),
        "selector_expression" | "index_expression" | "unary_expression" => node
            .child_by_field_name("operand")
            .and_then(|operand| target_base(operand, source)),
        "parenthesized_expression" => {
            let mut cursor = node.walk();
            let inner = node.named_children(&mut cursor).next();
            inner.and_then(|inner| target_base(inner, source))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lens_domain::trace_taint;
    use rstest::rstest;

    /// Lower `body` (a whole file after `package p` and the imports) and
    /// return `(kind, rule)` of every finding, resolving calls to
    /// same-file functions by bare name — enough of a call graph for the
    /// adapter's half of the contract.
    fn findings(body: &str) -> Vec<(String, String)> {
        let source = format!(
            "package p\n\nimport (\n\t\"database/sql\"\n\t\"encoding/json\"\n\t\"fmt\"\n\t\
             \"net/http\"\n\t\"os\"\n\t\"os/exec\"\n\t\"strconv\"\n\t\"strings\"\n\n\t\
             \"github.com/labstack/echo/v4\"\n)\n\nvar _ = sql.ErrNoRows\nvar _ = json.Marshal\n\
             var _ = fmt.Sprint\nvar _ = os.Open\nvar _ = strconv.Itoa\nvar _ = strings.Split\n\
             var _ = exec.Command\nvar _ echo.Context\nvar _ http.Handler\n\n{body}\n"
        );
        let flows = extract_taint_flows(&source, &[]).unwrap();
        let tree = parse_tree(&source).unwrap();
        let mut names = Vec::new();
        walk_top_level_fns(tree.root_node(), source.as_bytes(), &mut |site| {
            names.push(site.name.to_owned());
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
    #[case::query_parameter_into_a_shell(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tname := r.URL.Query().Get(\"name\")\n\
         \texec.Command(\"sh\", \"-c\", name).Run()\n}",
        one("command-injection", "os/exec.Command")
    )]
    #[case::sprintf_into_a_query(
        "func h(db *sql.DB, r *http.Request) {\n\
         \tq := fmt.Sprintf(\"SELECT * FROM t WHERE id = '%s'\", r.FormValue(\"id\"))\n\
         \tdb.Query(q)\n}",
        one("sql-injection", ".Query")
    )]
    #[case::bound_parameter_is_safe(
        "func h(db *sql.DB, r *http.Request) {\n\
         \tdb.Query(\"SELECT * FROM t WHERE id = ?\", r.FormValue(\"id\"))\n}",
        vec![]
    )]
    #[case::prepared_statement_takes_bind_parameters(
        "func h(db *sql.DB, r *http.Request) {\n\
         \tstmt, _ := db.Prepare(\"SELECT * FROM t WHERE id = ?\")\n\
         \tstmt.QueryRow(r.FormValue(\"id\"))\n}",
        vec![]
    )]
    #[case::query_text_after_a_context(
        "func h(db *sql.DB, r *http.Request) {\n\
         \tdb.QueryContext(r.Context(), \"SELECT 1 WHERE x = \" + r.FormValue(\"x\"))\n}",
        one("sql-injection", ".QueryContext")
    )]
    #[case::numeric_parse_sanitizes(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tn, _ := strconv.Atoi(r.FormValue(\"n\"))\n\
         \texec.Command(\"seq\", fmt.Sprint(n)).Run()\n}",
        vec![]
    )]
    #[case::json_decode_into_a_struct(
        "type req struct{ Path string }\n\
         func h(w http.ResponseWriter, r *http.Request) {\n\
         \tvar body req\n\
         \tjson.NewDecoder(r.Body).Decode(&body)\n\
         \tos.ReadFile(body.Path)\n}",
        one("path-traversal", "os.ReadFile")
    )]
    #[case::builder_receiver_is_written(
        "func h(db *sql.DB, r *http.Request) {\n\
         \tvar b strings.Builder\n\
         \tb.WriteString(\"SELECT * FROM t WHERE n = \")\n\
         \tb.WriteString(r.FormValue(\"n\"))\n\
         \tdb.Exec(b.String())\n}",
        one("sql-injection", ".Exec")
    )]
    #[case::a_field_write_does_not_taint_a_parameter_handle(
        "type C struct{ Data map[string]string; Dir string }\n\
         func h(c *C, r *http.Request) {\n\
         \tc.Data[\"next\"] = r.FormValue(\"next\")\n\
         \tos.ReadFile(c.Dir)\n}",
        vec![]
    )]
    #[case::an_allowlist_check_pins_the_value(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tmode := r.FormValue(\"mode\")\n\
         \tif mode == \"fast\" || mode == \"slow\" {\n\
         \t\texec.Command(\"run\", mode).Run()\n\t}\n}",
        vec![]
    )]
    #[case::an_allowlist_switch_pins_the_value(
        "func scheme(r *http.Request) string {\n\
         \tv := r.Header.Get(\"X-Forwarded-Proto\")\n\
         \tswitch v {\n\tcase \"http\", \"https\":\n\t\treturn v\n\t}\n\treturn \"\"\n}\n\
         func h(w http.ResponseWriter, r *http.Request) {\n\
         \thttp.Get(scheme(r) + \"://example.com\")\n}",
        vec![]
    )]
    #[case::an_inequality_pins_nothing(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tmode := r.FormValue(\"mode\")\n\
         \tif mode != \"\" || mode == \"slow\" {\n\
         \t\texec.Command(\"run\", mode).Run()\n\t}\n}",
        one("command-injection", "os/exec.Command")
    )]
    #[case::a_numeric_parameter_carries_nothing(
        "func remove(db *sql.DB, id int64) { db.Exec(fmt.Sprintf(\"DELETE FROM t WHERE id = %d\", id)) }\n\
         func h(w http.ResponseWriter, r *http.Request) {\n\
         \tid, _ := strconv.ParseInt(r.FormValue(\"id\"), 10, 64)\n\
         \tn := r.ContentLength\n\
         \tremove(nil, n + id)\n}",
        vec![]
    )]
    #[case::the_else_branch_is_not_pinned(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tmode := r.FormValue(\"mode\")\n\
         \tif mode == \"fast\" {\n\t\treturn\n\t} else {\n\
         \t\texec.Command(\"run\", mode).Run()\n\t}\n}",
        one("command-injection", "os/exec.Command")
    )]
    #[case::a_parenthesised_conjunct_pins(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tmode := r.FormValue(\"mode\")\n\
         \tif (mode == Fast) && r.Method != \"\" {\n\
         \t\texec.Command(\"run\", mode).Run()\n\t}\n}\n\
         const Fast = \"fast\"",
        vec![]
    )]
    #[case::a_package_constant_pins(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tm := r.FormValue(\"m\")\n\
         \tif m == http.MethodGet {\n\t\texec.Command(m).Run()\n\t}\n}",
        vec![]
    )]
    #[case::comparing_two_variables_pins_nothing(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \tm, other := r.FormValue(\"m\"), r.Method\n\
         \tif m == other {\n\t\texec.Command(m).Run()\n\t}\n}",
        one("command-injection", "os/exec.Command")
    )]
    #[case::a_plain_copy_of_a_db_is_not_prepared(
        "func h(db *sql.DB, r *http.Request) {\n\
         \td := db\n\
         \td.Query(r.FormValue(\"q\"))\n}",
        one("sql-injection", ".Query")
    )]
    #[case::closure_handler_parameter(
        "func routes() {\n\
         \thttp.HandleFunc(\"/\", func(w http.ResponseWriter, r *http.Request) {\n\
         \t\thttp.Redirect(w, r, r.FormValue(\"next\"), 302)\n\
         \t})\n}",
        one("open-redirect", "net/http.Redirect")
    )]
    #[case::echo_v4_context_is_a_source(
        "func h(c echo.Context) error {\n\
         \treturn exec.Command(c.QueryParam(\"cmd\")).Run()\n}",
        one("command-injection", "os/exec.Command")
    )]
    #[case::through_a_helper_and_back(
        "func id(r *http.Request) string { return r.FormValue(\"id\") }\n\
         func load(path string) { os.Open(path) }\n\
         func h(w http.ResponseWriter, r *http.Request) {\n\
         \tload(\"/data/\" + id(r))\n}",
        one("path-traversal", "os.Open")
    )]
    #[case::constant_arguments_are_clean(
        "func h(w http.ResponseWriter, r *http.Request) {\n\
         \texec.Command(\"ls\", \"-la\").Run()\n}",
        vec![]
    )]
    #[case::no_source_no_finding(
        "func run(cmd string) { exec.Command(cmd).Run() }",
        vec![]
    )]
    fn flows_from_go_source(#[case] body: &str, #[case] expected: Vec<(String, String)>) {
        assert_eq!(findings(body), expected);
    }

    #[test]
    fn a_shadowing_local_is_not_a_package_qualifier() {
        // `exec` is a local here, so `exec.Command` is a method call on it.
        let found = findings(
            "type runner struct{}\n\
             func (runner) Command(s string) {}\n\
             func h(w http.ResponseWriter, r *http.Request) {\n\
             \texec := runner{}\n\
             \texec.Command(r.FormValue(\"x\"))\n}",
        );
        assert!(found.is_empty(), "{found:?}");
    }

    #[test]
    fn parameters_map_to_their_slots_and_returns_are_recorded() {
        let source = "package p\n\nfunc f(a, b string, _ int, c string) (out string) {\n\
                      \tout = c\n\treturn\n}\n";
        let flows = extract_taint_flows(source, &[]).unwrap();
        assert_eq!(flows.len(), 1);
        assert_eq!(flows[0].returns, BTreeSet::from([Origin::Param(3)]));
        assert_eq!(flows[0].start_line, 3);
    }

    #[test]
    fn extra_source_types_extend_the_defaults() {
        let source = "package p\n\nimport (\n\t\"os/exec\"\n\tpb \"example.com/api/gen\"\n)\n\n\
                      func (s *S) Run(req *pb.RunRequest) { exec.Command(req.Cmd).Run() }\n";
        let without = extract_taint_flows(source, &[]).unwrap();
        assert!(without[0].sources.is_empty());
        let with =
            extract_taint_flows(source, &["example.com/api/gen.RunRequest".to_owned()]).unwrap();
        assert_eq!(with[0].sources.len(), 1);
        assert_eq!(with[0].sources[0].label, "req *pb.RunRequest");
    }

    /// The single function in `body`, lowered with `context`, `os/exec`
    /// and an example package imported.
    fn flow(body: &str) -> FunctionFlow {
        let source = format!(
            "package p\n\nimport (\n\t\"context\"\n\t\"net/http\"\n\n\t\
             \"example.com/foo\"\n)\n\nvar _ context.Context\nvar _ foo.Context\n\
             var _ http.Handler\n\n{body}\n"
        );
        let mut flows = extract_taint_flows(&source, &[]).unwrap();
        assert_eq!(flows.len(), 1, "{body}");
        flows.remove(0)
    }

    fn params(slots: &[usize]) -> BTreeSet<Origin> {
        slots.iter().map(|&slot| Origin::Param(slot)).collect()
    }

    /// The origins of each argument of the call to `g`.
    fn g_arguments(flow: &FunctionFlow) -> Vec<BTreeSet<Origin>> {
        flow.calls
            .iter()
            .filter(|call| call.callee_name.as_deref() == Some("g"))
            .flat_map(|call| call.arguments.clone())
            .collect()
    }

    #[test]
    fn only_context_contexts_and_numbers_are_clean_parameters() {
        let flow = flow(
            "func f(ctx context.Context, other foo.Context, cancel context.CancelFunc, n int64) {\n\
             \tg(ctx, other, cancel, n)\n}",
        );
        assert_eq!(
            g_arguments(&flow),
            [BTreeSet::new(), params(&[1]), params(&[2]), BTreeSet::new()]
        );
    }

    #[test]
    fn each_source_gets_its_own_index_and_declaration_line() {
        let flow = flow("func h(a,\n\tb *http.Request) string {\n\treturn b.Host\n}");
        assert_eq!(flow.sources.len(), 2);
        assert_eq!(flow.sources[1].label, "b *http.Request");
        assert_eq!(flow.sources[1].line, 14);
        assert_eq!(flow.returns, BTreeSet::from([Origin::Source(1)]));
    }

    #[rstest]
    #[case::short_var_pairs_by_position("a, b := \"lit\", x\n\tg(a, b)", vec![BTreeSet::new(), params(&[0])])]
    #[case::var_spec_pairs_by_position("var a, b = \"lit\", x\n\tg(a, b)", vec![BTreeSet::new(), params(&[0])])]
    #[case::type_switch_binds_its_alias("switch v := any(x).(type) {\n\tdefault:\n\t\tg(v)\n\t}", vec![BTreeSet::from([Origin::CallResult(0)])])]
    #[case::a_parameter_can_be_reassigned("y = x\n\tg(y)", vec![params(&[0, 1])])]
    #[case::a_field_write_taints_a_local("var v struct{ N string }\n\tv.N = x\n\tg(v)", vec![params(&[0])])]
    #[case::a_parenthesised_target_is_its_variable("var v string\n\t(v) = x\n\tg(v)", vec![params(&[0])])]
    #[case::an_address_argument_receives_the_call_inputs("var v string\n\tdec(x, &v)\n\tg(v)", vec![BTreeSet::from([Origin::CallInputs(0)])])]
    #[case::a_closure_argument_carries_nothing("g(func() { _ = x })", vec![BTreeSet::new()])]
    fn lowering_writes(#[case] body: &str, #[case] expected: Vec<BTreeSet<Origin>>) {
        let flow = flow(&format!("func f(x, y string) {{\n\t{body}\n}}"));
        assert_eq!(g_arguments(&flow), expected);
    }

    #[test]
    fn a_closure_return_is_not_the_function_return() {
        let flow = flow(
            "func f(x string) string {\n\th := func() string { return x }\n\t_ = h\n\treturn \"\"\n}",
        );
        assert!(flow.returns.is_empty(), "{:?}", flow.returns);
    }

    #[rstest]
    #[case::generic_instantiation("F[K]()", "F")]
    #[case::parenthesised_callee("(g2)(x)", "g2")]
    fn callee_names_see_through_instantiation_and_parens(#[case] call: &str, #[case] name: &str) {
        let flow = flow(&format!("func f(x string) {{\n\t{call}\n}}"));
        assert_eq!(
            flow.calls.first().and_then(|c| c.callee_name.as_deref()),
            Some(name),
            "{flow:?}"
        );
        assert_eq!(flow.calls[0].line, 15);
    }

    #[rstest]
    #[case(Checked::All, 2, vec![0, 1])]
    #[case(Checked::At(1), 2, vec![1])]
    #[case(Checked::At(2), 2, vec![])]
    #[case(Checked::From(1), 3, vec![1, 2])]
    fn checked_positions(
        #[case] checked: Checked,
        #[case] count: usize,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(checked.positions(count), expected);
    }

    #[rstest]
    #[case("net/http", Some("http"))]
    #[case("example.com/go.uber", Some("go.uber"))]
    #[case("github.com/labstack/echo/v4", Some("echo"))]
    #[case("gopkg.in/yaml.v3", Some("yaml"))]
    #[case("example.com/v2go", Some("v2go"))]
    #[case("", None)]
    fn default_package_names(#[case] path: &str, #[case] expected: Option<&str>) {
        assert_eq!(default_package_name(path).as_deref(), expected);
    }
}

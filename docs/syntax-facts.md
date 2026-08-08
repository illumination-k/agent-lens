# Cross-language syntax facts

`agent-lens` analyzers should exchange language-neutral syntax facts instead
of making every adapter look like Rust. The shared model lives in
`lens_domain::syntax` and is intentionally syntax-only: adapters populate facts
available from their lightweight parser, while semantic backends can enrich the
same facts later.

## Missing facts

Fields that vary by language or parser backend use `SyntaxFact<T>`.

- `Known(value)`: the adapter determined the fact.
- `Known(None)`: the adapter determined that an optional fact is absent.
- `Unknown`: the adapter did not determine the fact and callers must not guess.

This distinction matters for owners, receivers, visibility, return types, and
resolution. For example, a top-level Rust function has `owner = Known(None)`;
a future adapter that does not track owners should use `owner = Unknown`.

## FunctionShape

`FunctionShape` is the common function-like unit for graph and similarity
analyzers. It carries:

- display name and optional qualified name;
- module/package path;
- class, impl, trait, receiver, namespace, or module owner;
- visibility/export status;
- signature facts;
- doc text (Rust `///` / `#[doc]`, Python docstring, Go doc comment, TS
  JSDoc) with comment markers stripped, `None` when absent or not
  extracted;
- body tree;
- source span and test classification.

The body tree remains the existing `TreeNode` currency for structural
similarity, but it is now nested under a neutral shape so language adapters can
add comparable signature and ownership facts without CLI-specific structs.

The lighter `FunctionDef` (the similarity corpus unit) additionally carries
`implements`: the trait a definition implements, where the language marks it
syntactically — the trait path's last identifier for a Rust
`impl Trait for Type` method, `None` everywhere else (Go satisfies interfaces
structurally, so no Go definition carries it). Similarity scoring uses it to
drop the signature component on same-trait pairs: two implementations of one
trait share a signature by construction, so that match is not evidence of
duplication.

## TypeShape

`TypeShape` (`lens_domain::type_shape`) is the type-definition unit compared
by `analyze similarity --target types`. Kinds are language-neutral:

- `Record` — named fields or properties: Rust struct, TS interface or
  object-literal type alias, Python annotated class (dataclass / TypedDict),
  Go struct;
- `Enum` — tagged variants: Rust/TS enums, Python `Enum` subclasses;
- `Alias` — a name for another type: `type X = …` and Go defined types.

The language-facing spelling lives in `kind_label`, a fixed vocabulary
(`struct`, `enum`, `type_alias`, `interface`, `class`, `dataclass`) surfaced
in reports. Members carry the declared name, the normalized annotation text,
and the referenced type paths.

A TS interface member is a member whatever its spelling: a method, call, or
construct signature contributes the arrow type a property holding it would
declare (`load(): void` and `load: () => void` are one declaration in
TypeScript and produce one member), and an index signature contributes
`[string]: unknown` with the key binding's local name dropped. This is what
lets a method-only contract — the normal shape of a repository or service
interface — compare on its method set.

`is_shapeless()` marks a definition with no members and no variants. Such a
shape renders as a bare root node, so it does not score _highly_ against
another shapeless definition, it scores _vacuously_, at 1.0, against every
one of them; the similarity corpus drops these before pairing rather than
let a marker interface, a unit struct, and an empty enum form one cluster.
`--min-lines` cannot stand in for this — an empty definition can still be
spelled across several lines.

Rendering into the comparison currency lives in the domain, not the
adapters, so every language emits one label vocabulary: the root label is
the neutral kind, each member becomes a leaf labelled
`Field(name: type_text)` with names case-folded (`userId` = `user_id`) and
type text whitespace-normalized, enum variants become `Variant(name)` nodes
with payload `Field` children. Member facts go into labels — never
`TreeNode::value` — because APTED, token profiles, and exact-match hashing
compare labels only. `member_signature()` synthesizes a `SignatureShape`
from the member list so the signature component of the similarity blend
(identifier overlap, type overlap, member count) applies to type pairs
unchanged, and `into_function_shape()` lowers the whole definition into the
`FunctionShape` corpus currency the similarity pipeline runs on.

## BlockShape

`BlockShape` (`lens_domain::block_shape`) is the sub-function unit compared by
`analyze similarity --target blocks`: a contiguous run of statements inside a
function body.

Adapters supply `StatementSeq`s — one per statement list in the file, at every
nesting depth (function body, `if` arm, loop body, `switch` case, `match` arm),
each `StatementUnit` carrying its 1-based inclusive line span and the same
subtree the adapter would nest under a function body's `Block`. Reusing the
body lowering is what makes a window covering a whole body identical to that
body's own tree.

Windowing lives in the domain, not the adapters, so every language produces
the same unit population for the same shape of code. `block_windows` slides
over each list, minting one window per contiguous run of up to
`DEFAULT_MAX_WINDOW_STATEMENTS` (8) statements, and drops a window that

- spans fewer than `min_lines` source lines (the `--min-lines` cut), or
- lowers to fewer than `MIN_WINDOW_TREE_NODES` (8) tree nodes, or
- repeats a source span already emitted for that file.

The node floor matters because line count is a poor size proxy here: a Rust
`matches!` body lowers to one `MacroStmt` leaf however many lines it spans, and
two such windows would score a perfect 1.0 against each other. Capping the run
length keeps the unit count linear in the statement count
(`statements × max_statements`) rather than quadratic.

`into_function_shape()` lowers a window into the `FunctionShape` corpus
currency, with the window tree as the body and no signature — blocks have
nothing to compare there, so the analyzer scores them on the body alone
instead of treating a missing signature as a perfect match.

## SignatureShape

`SignatureShape` records comparable syntax where available:

- parameter names, annotations, and type paths;
- return annotation/type paths;
- receiver kind;
- generics and bounds;
- identifier tokens for signature-aware similarity.

Languages without annotations should use `Known(None)` for individual missing
annotations when they know the parameter exists, and `Unknown` only when the
adapter does not inspect that part of the syntax.

## InterfaceShape

`InterfaceShape` records a named interface-like declaration — Go `interface`
today; a Rust `trait` would project the same way — as its directly declared
method set: per method, the name and the parameter-slot count (grouped names
expanded, a variadic slot counting one). Embedded interfaces are not expanded:
an embed declared in the analyzed tree contributes its methods through its own
declaration, and an out-of-scope embed has no visible method set anyway.

The visibility analyzer matches exported methods against these sets by name
and arity to annotate rows whose calls can dispatch through an interface. The
same sets could later feed the call-graph resolver (interface-aware candidate
sets for receiver calls), so the extraction is not analyzer-specific.

## CallShape

`CallShape` records:

- caller qualified name and caller module;
- callee display name and path segments;
- receiver expression kind;
- whether the callee name is bound in the caller's local scope;
- lexical resolution status;
- imports visible at the call site;
- source line.

Default extraction should set resolution to `NotAttempted`. Graph analyzers can
then fold language-specific lexical rules into `Resolved`, `Unresolved`, or
`Ambiguous` without requiring type inference.

## Locally bound callees

A bare call whose callee is bound in the caller's own scope — a closure or
nested function held in a local, a function-typed parameter — targets that
binding, which shadows every definition outside the function. Adapters that can
read local scopes set `callee_is_locally_bound` to `Known(true)` for such call
sites, and the call-graph resolver leaves them unresolved rather than matching
the name against the workspace.

Without it, `emit := func(...) {...}; emit(ev)` resolves through the
last-segment fallback to whichever module happens to define an `emit`,
fabricating a cross-module edge — enough to turn cleanly layered modules into a
reported cycle in `layers` and to inflate fan-in in `hubs`. Unlike the
ubiquitous-name and builtin tables below, this needs no name list: scope is
decidable from the AST the adapter already walks.

Adapters recognise, per language: Go `:=` / `var` / `=` bindings to a
`func_literal`, `func`-typed `var`s and parameters; Rust `let` bindings to a
closure, nested `fn` items, `fn`-typed locals, and parameters of
`Fn`/`FnMut`/`FnOnce`/`fn` type (directly, via a generic bound, or behind a
reference or wrapper); TypeScript arrow/function-expression locals, nested
`function` declarations, and parameters with a function-type annotation or a
function default; Python nested `def`s, `lambda` locals, and `Callable`
annotations. Bindings are collected body-wide rather than from the point of
declaration — a call that precedes its binding and means an outer name is rare,
and losing an edge beats fabricating one. An adapter that does not track scopes
leaves the fact `Unknown`, which reads as "not locally bound".

## ImportShape

`ImportShape` records imported module, local alias, and exported/re-exported
symbol when the language exposes them. Rust currently maps visible `use` aliases
into this shape for function graph lexical resolution.

## Ubiquitous method names

A receiver call (`recv.foo()`) carries no type, so the only fact the call-graph
resolver has to work with is the callee name. That is enough for a name a
workspace invented and worthless for one the standard library defines on nearly
every value — `.clone()`, `.map()`, `.append()`, `.String()` — where a name match
against a workspace function produces a phantom edge rather than a lucky hit.

Each adapter therefore exports `UBIQUITOUS_METHOD_NAMES`, a
`lens_domain::UbiquitousMethodNames` table of the names in its language that
carry no attribution evidence. The resolver consults the table of the language
the call site was extracted from, and leaves such receiver calls `Unresolved`.
The tables belong next to the adapters because the conventions are per-grammar;
`lens_domain` owns only the lookup shape.

The table gates receiver calls only. A call whose syntax carries the owner —
a typed path (`Foo::clone(x)`, `W.map(w)`) or `self.method()` — resolves
normally, since the evidence is in the call site rather than in the name.

An entry belongs in a table when the language's standard library defines the
name on several unrelated types. Names a project might plausibly own stay out:
dropping them costs real edges, and the resolver's true positives come almost
entirely from workspace-specific names.

## Adapter migration

Current migration state:

- Rust function graph reads `FunctionShape` and `CallShape` first.
- Similarity stores `FunctionShape` in its corpus and scores through
  neutral body/signature facts.
- Other language adapters can continue producing `FunctionDef` while they are
  migrated; `FunctionShape::from(FunctionDef)` preserves body similarity and
  marks unavailable facts as `Unknown`.

Future enrichment should attach facts to this model rather than replacing the
lightweight parser path. Good enrichment sources include rust-analyzer,
TypeScript language service, pyright/jedi, and gopls.

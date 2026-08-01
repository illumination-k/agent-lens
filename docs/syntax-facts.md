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

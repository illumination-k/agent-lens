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
- lexical resolution status;
- imports visible at the call site;
- source line.

Default extraction should set resolution to `NotAttempted`. Graph analyzers can
then fold language-specific lexical rules into `Resolved`, `Unresolved`, or
`Ambiguous` without requiring type inference.

## ImportShape

`ImportShape` records imported module, local alias, and exported/re-exported
symbol when the language exposes them. Rust currently maps visible `use` aliases
into this shape for function graph lexical resolution.

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

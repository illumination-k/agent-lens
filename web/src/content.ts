/**
 * Copy for the landing page.
 *
 * Kept out of the component so the same rows feed both the rendered section
 * and the structured data (the FAQ block is rendered *and* emitted as
 * `FAQPage` JSON-LD — two copies of that text would drift apart, and Google
 * treats a mismatch between the two as a reason to drop the rich result).
 *
 * Everything here restates `README.md`. When an analyzer changes, the README
 * table is the source both sides are corrected against.
 */
import type { FaqEntry } from "./seo";

export const TAGLINE =
  "A sharper view of your codebase, tuned for the agent that's about to edit it.";

export const INSTALL_COMMAND =
  "curl -fsSL https://raw.githubusercontent.com/illumination-k/agent-lens/main/install.sh | bash";

/**
 * A real run, captured from this repository's own sources — the project
 * dogfoods every analyzer through `mise run selftest`, so the sample on the
 * page is the output the command actually prints rather than a mock-up.
 */
export const SAMPLE_REPORT = `$ agent-lens analyze similarity crates/lens-rust/src --format md --top 2 --exclude-tests

# Similarity report: crates/lens-rust/src (tsed method, 219 function(s), threshold 0.85, min lines 5)

## Top 2 similar cluster(s) of 2 total

- 2 functions, similarity 89–89%, identifier overlap 33–33%
  - cohesion.rs:\`extract_cohesion_units\` (L62-67)
  - type_defs.rs:\`extract_type_defs\` (L16-21)

- 3 functions, similarity 89–89%, identifier overlap 14–17%
  - call_index.rs:\`bound_is_fn_trait\` (L509-516)
  - cohesion.rs:\`is_self_expr\` (L218-226)
  - complexity.rs:\`is_rust_keyword\` (L194-235)`;

export type Pillar = {
  title: string;
  body: string;
  command: string;
};

/** The three surfaces the CLI bundles, in the order the README introduces them. */
export const PILLARS: readonly Pillar[] = [
  {
    title: "Hooks",
    body: "Handlers that speak the Claude Code and Codex hook protocols on stdin/stdout. The agent gets a hotspot thumbnail at session start, a complexity and cohesion heads-up on the file it is about to edit, and a duplicate and wrapper report on the file it just changed. Advisory, never a gate: a failing hook still answers in the agent's own response schema and exits 0.",
    command: "agent-lens hook setup",
  },
  {
    title: "Analyzers",
    body: "Eighteen on-demand analyses of code shape — duplication, complexity, coupling, call-graph structure, change risk. JSON on stdout by default, compact Markdown with --format md, both sized to drop straight into a prompt.",
    command: "agent-lens analyze similarity src --format md",
  },
  {
    title: "Profiles and baselines",
    body: "Name a repeatable multi-analyzer pass in agent-lens.toml and run it in one command. A baseline reduces that run to a handful of named numbers, so a repository can adopt a threshold without first paying off the debt it already has — only regressions fail.",
    command: "agent-lens run web",
  },
];

export type BlindSpot = string;

/** The "why" section: what an agent cannot see from the file it has open. */
export const BLIND_SPOTS: readonly BlindSpot[] = [
  "The near-duplicate function three modules over that it is about to fork.",
  "The impl block whose methods touch disjoint sets of fields and should be split.",
  "The module that is a Fan-In bottleneck and should not grow any more.",
  "The function whose Cognitive Complexity is 40 and is a landmine to refactor.",
];

export type Analyzer = {
  name: string;
  summary: string;
};

export type AnalyzerGroup = {
  title: string;
  blurb: string;
  analyzers: readonly Analyzer[];
};

export const ANALYZER_GROUPS: readonly AnalyzerGroup[] = [
  {
    title: "Duplication and indirection",
    blurb: "What already exists, and how many hops sit between the caller and the work.",
    analyzers: [
      {
        name: "similarity",
        summary:
          "Near-duplicate pairs by TSED tree-edit distance over normalised ASTs, folded into clusters. --target picks functions, type definitions, or statement blocks inside function bodies.",
      },
      {
        name: "wrapper",
        summary:
          "Functions whose body is a forwarding call modulo a short chain of ?, .unwrap(), .into(), .await.",
      },
      {
        name: "delegation",
        summary:
          "Chains that only forward — api::save -> service::save -> repo::save -> db::insert — with the terminus that does the work as the headline and a per-module lasagna roll-up.",
      },
    ],
  },
  {
    title: "Shape of a module",
    blurb: "How much a unit is holding, and how much has to be read to reason about it.",
    analyzers: [
      {
        name: "complexity",
        summary:
          "Per-function Cyclomatic, Cognitive, Max Nesting Depth, Halstead Volume, and Maintainability Index.",
      },
      {
        name: "cohesion",
        summary:
          "LCOM4 per impl block, class, or module unit: the number of connected components in the field-sharing graph.",
      },
      {
        name: "coupling",
        summary:
          "Module-level Fan-In, Fan-Out, Henry-Kafura IFC, Martin's Instability, per-pair shared symbols, and the cycles.",
      },
      {
        name: "context-span",
        summary:
          "The transitive dependency closure per module — how many files an agent must read before it can reason about one.",
      },
    ],
  },
  {
    title: "Call graph",
    blurb: "Who calls what, what a change reaches, and which layers were crossed to get there.",
    analyzers: [
      {
        name: "function-graph",
        summary:
          "Nodes and heuristic caller-to-callee edges as visualization-ready JSON, weighted by calls, fan-in/out, LOC, complexity, and MI.",
      },
      {
        name: "cycles",
        summary:
          "Function-level strongly connected components with advisory cheapest-cut break suggestions and call-line evidence.",
      },
      {
        name: "hubs",
        summary:
          "Outlier fan-out god functions, outlier fan-in blast-radius carriers, Henry-Kafura bottlenecks, and cross-module pull.",
      },
      {
        name: "layers",
        summary:
          "Inferred Lakos levelization, entry points, module cycles, and skip-level calls with call-site evidence.",
      },
      {
        name: "impact",
        summary:
          "Blast radius of the working-tree diff or a named function: transitive callers folded per depth, plus the reachable tests as a verification checklist.",
      },
      {
        name: "graph-query",
        summary:
          "One canned traversal per run — callers, callees, neighborhood, or the shortest path between two symbols.",
      },
    ],
  },
  {
    title: "Reachability",
    blurb: "Code nothing runs, nothing tests, or nothing outside the module needs.",
    analyzers: [
      {
        name: "untested",
        summary:
          "Production functions with no resolved call path from any test, grouped by module and ranked by untested LOC.",
      },
      {
        name: "unreachable",
        summary:
          "Functions no entry point reaches, in confidence tiers — confirmed rows are deletable on that evidence alone (Rust, Go).",
      },
      {
        name: "visibility",
        summary:
          "pub or exported functions whose callers all sit inside a narrower scope, with the declaration that would still compile (Rust, Go).",
      },
    ],
  },
  {
    title: "Change risk",
    blurb: "Where git history and code shape agree that an edit is expensive.",
    analyzers: [
      {
        name: "hotspot",
        summary:
          "Files ranked by commits x cognitive_max over a --since window: where churn and complexity overlap.",
      },
      {
        name: "risk",
        summary:
          "The rank product of churn and call-graph centrality, so hot and load-bearing outranks hot but leaf.",
      },
    ],
  },
];

export type LanguageRow = {
  language: string;
  parser: string;
  coverage: string;
};

export const LANGUAGES: readonly LanguageRow[] = [
  { language: "Rust", parser: "syn", coverage: "Every analyzer" },
  {
    language: "TypeScript / JavaScript",
    parser: "oxc",
    coverage: "All but unreachable, visibility",
  },
  { language: "Python", parser: "ruff_python_parser", coverage: "All but unreachable, visibility" },
  { language: "Go", parser: "tree-sitter", coverage: "Every analyzer" },
];

export type InstallOption = {
  title: string;
  note: string;
  command: string;
};

export const INSTALL_OPTIONS: readonly InstallOption[] = [
  {
    title: "Install script",
    note: "Linux x86_64 / arm64 (glibc or musl) and macOS arm64 / x86_64. Pulls the matching tarball from the latest release, verifies its SHA-256, and drops the binary into ~/.local/bin.",
    command: INSTALL_COMMAND,
  },
  {
    title: "mise",
    note: "Straight from GitHub Releases, no Rust toolchain, pinned per project.",
    command: "mise use -g github:illumination-k/agent-lens",
  },
  {
    title: "Nix flake",
    note: "Built from source and pinned by flake.lock — no release artifact involved.",
    command: "nix run github:illumination-k/agent-lens -- --version",
  },
  {
    title: "From source",
    note: "The workspace is on edition 2024, so rustc 1.85 or newer.",
    command: "cargo install --path crates/agent-lens",
  },
];

export const FAQ: readonly FaqEntry[] = [
  {
    question: "How is agent-lens different from a linter?",
    answer:
      "A linter tells a human how to write nicer code, one file at a time. agent-lens answers repository-scale questions an LLM asks before it edits — which functions duplicate this one, how tangled is this module, what breaks if I change this — and emits structured reports sized for a context window rather than terminal decoration.",
  },
  {
    question: "Which languages does it analyze?",
    answer:
      "Rust, TypeScript / JavaScript, Python, and Go. Every analyzer runs on all four except unreachable and visibility, which need extracted export status and are wired through the Rust and Go adapters only. Analysis is split into a language-neutral core and per-language adapters, so adding a language means writing one adapter crate rather than reimplementing the metrics.",
  },
  {
    question: "Do I have to use it through a coding agent?",
    answer:
      "No. Every analyzer is an ordinary CLI subcommand: agent-lens analyze <tool> <path>. The hook handlers are one way to deliver that output automatically, and agent-lens run <profile> is another for CI and pre-commit passes.",
  },
  {
    question: "Does a failing hook block my agent?",
    answer:
      "No. Hook handlers are advisory. A handler that fails still answers in the agent's own response schema, prefixed with 'agent-lens <event> hook failed:', and exits 0 so the agent parses it. The full error goes to stderr.",
  },
  {
    question: "Is it stable?",
    answer:
      "Not yet. The project is pre-alpha: the CLI details and report schemas are still allowed to change without a major version bump while the tool settles.",
  },
  {
    question: "What does it cost?",
    answer:
      "Nothing. agent-lens is open source under the MIT license, distributed as a single static binary with no service, account, or telemetry attached.",
  },
];

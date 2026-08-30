//! `after_long_help` blocks for the CLI.
//!
//! Every string here is written to read the same way twice: as terminal
//! text under `--help`, and as Markdown once `help --md` splices it into
//! the reference document. That is why command listings are indented by
//! four spaces — a plain aligned block in the terminal, a fenced-free
//! code block in Markdown — while explanations stay unindented prose.
//!
//! Keep the lines short. clap re-wraps prose to the terminal width but
//! leaves the indented blocks alone, so an over-long example line is the
//! one thing that will look wrong.

/// The question-to-analyzer routing table. Shared by the root help and
/// `analyze --help` so the choice is in front of an agent at both of the
/// points where it is actually being made. A macro rather than a `const`
/// so `concat!` can splice it into both.
macro_rules! routing {
    () => {
        "\
Pick an analyzer by question:

    is this file in the right module?     analyze communities
    where should I refactor first?        analyze hotspot
    how carefully should I edit this?     analyze risk
    what else will I have to edit?        analyze co-change
    what couples these without saying so? analyze hidden-coupling
    did this edit end up scattered?       analyze change-entropy
    is this function too complex?         analyze complexity
    where does this codebase do X?        analyze search
    did I already write this?             analyze similarity
    is this body just a forwarder?        analyze wrapper
    is one caller all this function has?  analyze single-use
    is one impl all this trait has?       analyze single-impl
    how many hops before real work?       analyze delegation
    does this type do too many things?    analyze cohesion
    which modules are entangled?          analyze coupling
    which functions recurse mutually?     analyze cycles
    how much must I read to edit this?    analyze context-span
    which functions are load-bearing?     analyze hubs
    what breaks if I change this?         analyze impact
    which level does this code sit on?    analyze layers
    what has no test path guarding it?    analyze untested
    what do only tests keep alive?        analyze test-only
    can anything still reach this code?   analyze unreachable
    is this `pub` wider than it needs?    analyze visibility
    who calls this / how do I get there?  analyze graph-query
    give me the raw call graph            analyze function-graph
"
    };
}

/// The output conventions every analyzer obeys, stated once instead of
/// repeated per subcommand.
macro_rules! conventions {
    () => {
        "\
Reports go to stdout; logs and diagnostics go to stderr via `tracing` (set
RUST_LOG to change the level). Analyzers default to `--format json`;
`--format md` is the compact rendering tuned for LLM context. Exit status is
0 on success and 1 on failure.
"
    };
}

/// Root help: how to choose an analyzer, plus the output conventions that
/// hold for all of them. An agent that reads only this block should be
/// able to pick the right subcommand without running `--help` again.
pub const ROOT: &str = concat!(
    conventions!(),
    "\n",
    routing!(),
    "
Examples:

    agent-lens analyze hotspot . --format md --top 15
    agent-lens analyze similarity packages cli --format md
    agent-lens hook setup --dry-run     # wire hooks into settings.json
    agent-lens run audit                # a profile from agent-lens.toml
    agent-lens baseline create audit    # that profile's metrics, snapshotted
    agent-lens help --md                # whole CLI as one Markdown doc
"
);

/// `analyze --help`: the same routing table, plus the path filters every
/// analyzer shares, so the group help answers "which one" without a
/// second `--help` round-trip.
pub const ANALYZE: &str = concat!(
    conventions!(),
    "
Every analyzer takes the same path filters: `--only-tests`,
`--exclude-tests`, and a repeatable `--exclude <GLOB>`.

Every analyzer but `coupling` and `context-span` takes more than one PATH
— `analyze similarity packages cli web/src` walks all three into one
report, which is what finds a duplicate spanning two of them. Display
paths are written relative to the paths' deepest common ancestor.
`coupling` and `context-span` grow a module graph out of a single entry
point, so they keep the one-PATH signature.

`--top` bounds the length of a `--format md` report. Every analyzer that
can produce a long one accepts it; `cycles` does not, because a truncated
cycle list reads as the whole list.

",
    routing!()
);

pub const COHESION: &str = "\
Examples:

    agent-lens analyze cohesion src/ --format md
    agent-lens analyze cohesion src/parser.rs --min-score 1 --format md
    agent-lens analyze cohesion . --diff-only --format md
";

pub const COMPLEXITY: &str = "\
Examples:

    agent-lens analyze complexity src/ --format md --top 10
    agent-lens analyze complexity src/cli.rs --min-score 15 --format md
    agent-lens analyze complexity . --diff-only --format md
    agent-lens analyze complexity . --diff-range HEAD~1..HEAD --format md
";

pub const COUPLING: &str = "\
Examples:

    agent-lens analyze coupling src/lib.rs --format md --top 15
    agent-lens analyze coupling ./crates/core --format md
    agent-lens analyze coupling src/index.ts --exclude-tests --format md
    agent-lens analyze coupling ./src/mypkg --format md
";

pub const CYCLES: &str = "\
Examples:

    agent-lens analyze cycles src/ --format md
    agent-lens analyze cycles src/ --exclude-tests --format md
";

pub const FUNCTION_GRAPH: &str = "\
Examples:

    agent-lens analyze function-graph src/ --format md
    agent-lens analyze function-graph src/ > graph.json
";

pub const GRAPH_QUERY: &str = "\
Examples:

    agent-lens analyze graph-query src/ --query callers \\
      --symbol Resolver::resolve --format md
    agent-lens analyze graph-query src/ --query callees \\
      --symbol main --depth 2 --format md
    agent-lens analyze graph-query src/ --query neighborhood \\
      --symbol parse --direction both --depth 2 --format md
    agent-lens analyze graph-query src/ --query path \\
      --symbol main --to write_stdout_json --format md
";

pub const CONTEXT_SPAN: &str = "\
Examples:

    agent-lens analyze context-span src/lib.rs --format md
    agent-lens analyze context-span . --entry-glob 'app/**/page.tsx' \\
      --entry-glob 'app/**/route.ts' --format md
";

pub const HUBS: &str = "\
Examples:

    agent-lens analyze hubs src/ --format md --top 10
    agent-lens analyze hubs src/ --exclude-tests --format md
";

pub const SINGLE_IMPL: &str = "\
Examples:

    agent-lens analyze single-impl src/ --format md
    agent-lens analyze single-impl src/ --top 10 --format md
";

pub const SINGLE_USE: &str = "\
Examples:

    agent-lens analyze single-use src/ --format md
    agent-lens analyze single-use src/ --max-loc 12 --max-cyclomatic 4 --format md
";

pub const IMPACT: &str = "\
Examples:

    agent-lens analyze impact src/ --format md
    agent-lens analyze impact src/ --function Resolver::resolve --format md
    agent-lens analyze impact src/ --function parse --function render \\
      --depth 3 --format md
";

pub const LAYERS: &str = "\
Examples:

    agent-lens analyze layers src/ --format md
    agent-lens analyze layers src/ --exclude-tests --top 30 --format md
";

pub const TEST_ONLY: &str = "\
Examples:

    agent-lens analyze test-only src/ --format md
    agent-lens analyze test-only src/ --top 10 --format md
";

pub const UNTESTED: &str = "\
Examples:

    agent-lens analyze untested src/ --format md
    agent-lens analyze untested . --exclude 'benches/**' --top 30 --format md
";

pub const UNREACHABLE: &str = "\
Examples:

    agent-lens analyze unreachable . --format md
    agent-lens analyze unreachable crates/ --tier unknown --format md
    agent-lens analyze unreachable . --exclude 'benches/**' --top 30 --format md
";

pub const DELEGATION: &str = "\
Examples:

    agent-lens analyze delegation . --format md
    agent-lens analyze delegation crates/ --top 30 --format md
    agent-lens analyze delegation . --diff-only --format md
";

pub const VISIBILITY: &str = "\
Examples:

    agent-lens analyze visibility . --format md
    agent-lens analyze visibility crates/ --top 30 --format md
    agent-lens analyze visibility . --exclude-tests --format md
";

pub const HOTSPOT: &str = "\
Examples:

    agent-lens analyze hotspot src/ --format md --top 15
    agent-lens analyze hotspot . --since 90.days.ago --format md
    agent-lens analyze hotspot . --exclude-tests --exclude '*.gen.ts'
";

pub const RISK: &str = "\
Examples:

    agent-lens analyze risk . --format md --top 15
    agent-lens analyze risk . --since 90.days.ago --format md
    agent-lens analyze risk crates/ --exclude-tests --format md
";

pub const CO_CHANGE: &str = "\
Point it at the widest path you care about: a pathspec scopes the file sets
as well as the commits, so `src/` ↔ `docs/` pairs only show up in a run
covering both.

Examples:

    agent-lens analyze co-change . --format md --top 15
    agent-lens analyze co-change . --since 180.days.ago --format md
    agent-lens analyze co-change . --min-support 5 --min-confidence 0.7
    agent-lens analyze co-change . --max-commit-files 20 --format md
";

pub const CHANGE_ENTROPY: &str = "\
Point it at the widest path you care about: a pathspec scopes the change sets
too, so a run over one directory measures only the part of each change that
landed in it.

Before a commit, reach for --diff-only: it reads the pending change instead of
the history and says where its scatter sits among the commits this repository
actually makes.

Examples:

    agent-lens analyze change-entropy . --diff-only --format md
    agent-lens analyze change-entropy . --format md --top 15
    agent-lens analyze change-entropy . --since 180.days.ago --period month
    agent-lens analyze change-entropy . --diff-range HEAD~1..HEAD --format md
    agent-lens analyze change-entropy . --min-commits 5 --max-commit-files 20
";

pub const COMMUNITIES: &str = "\
Read the two modularity scores first: a declared score close to the detected
one means the directory structure already is the clustering, and the misfiled
rows below it are noise. A wide gap is what makes them worth reading.

Examples:

    agent-lens analyze communities crates/agent-lens --format md
    agent-lens analyze communities crates/agent-lens --granularity module --format md
    agent-lens analyze communities src/index.ts --min-community 3 --top 10
";

pub const HIDDEN_COUPLING: &str = "\
The differential needs both halves in scope: the history is scoped by the same
pathspec as the file sets, and the static graph is grown from the paths given.
Point it at the widest path you care about, and prefer a repo with full history
— a shallow clone hides the evidence the suspect bucket is looking for.

Examples:

    agent-lens analyze hidden-coupling . --format md --top 15
    agent-lens analyze hidden-coupling . --since 180.days.ago --format md
    agent-lens analyze hidden-coupling . --min-support 5 --min-confidence 0.7
    agent-lens analyze hidden-coupling crates --exclude-tests --format md
";

pub const SEARCH: &str = "\
Examples:

    agent-lens analyze search crates/ --query 'diff range gate' --format md
    agent-lens analyze search . --query parse_diff_range --limit 5 --format md
    agent-lens analyze search . --query 'retry backoff' --rank graph --format md
    agent-lens analyze search . --query changed_line_rangs --format md
    agent-lens analyze search . --query retry --fuzzy always --format md
    agent-lens analyze search . --query hunk --fuzzy off --exclude-tests
";

pub const SIMILARITY: &str = "\
Examples:

    agent-lens analyze similarity src/ --format md
    agent-lens analyze similarity packages cli web/src --format md
    agent-lens analyze similarity src/ --sweep 0.6,0.75,0.85 --format md
    agent-lens analyze similarity . --diff-only --format md
    agent-lens analyze similarity . --diff-range main...HEAD --format md
    agent-lens analyze similarity src/ --doc-overlap --format md
    agent-lens analyze similarity src/ --method token --min-lines 10
    agent-lens analyze similarity . --paired-by name --format md
    agent-lens analyze similarity src/ --target types --format md
    agent-lens analyze similarity . --target types --paired-by name --format md
";

pub const WRAPPER: &str = "\
Examples:

    agent-lens analyze wrapper src/ --format md --top 30
    agent-lens analyze wrapper packages cli --format md
    agent-lens analyze wrapper . --diff-only --format md
";

pub const RUN: &str = "\
Run `agent-lens config schema` for the keys a profile accepts.

Examples:

    agent-lens run audit
    agent-lens run audit --config ./agent-lens.toml
    agent-lens run audit --digest    # one row per file, drill-down commands
";

pub const BASELINE: &str = "\
A baseline snapshots the profile's analyzers as named numbers, so a later
run can tell a regression from debt that was already there. Covered
analyzers: complexity, cohesion, coupling, context-span, hotspot, and
similarity; anything else in the profile is listed under `skipped`.

The document is deterministic — same tree, same commit, same bytes — so it
is safe to store as a CI artifact and diff.

`compare` re-runs the profile and judges each metric by its own
direction. Surface-size figures (file/function/unit/module counts,
`loc_total`, `edge_count`) and git-history figures (`commits_max`,
`score_max`) are reported when they move but never gate: a bigger
codebase and an extra commit are not regressions. It exits 2 when
something gated moved the wrong way, so a CI step can tell that from the
1 a failure to run exits with. `--update` makes it a ratchet: what
improved is written back, what regressed keeps its stored value.

Examples:

    agent-lens baseline create audit
    agent-lens baseline create audit --out target/agent-lens/baseline.json
    agent-lens baseline compare audit baseline.json --format md
    agent-lens baseline compare audit baseline.json --update
";

pub const HOOK_SETUP: &str = "\
Examples:

    agent-lens hook setup --dry-run
    agent-lens hook setup --scope user
";

pub const CODEX_HOOK_SETUP: &str = "\
Examples:

    agent-lens codex-hook setup --dry-run
    agent-lens codex-hook setup --scope project
";

pub const SKILLS: &str = "\
Examples:

    agent-lens skills list
    agent-lens skills install --dry-run
    agent-lens skills install --scope user --force
";

pub const CONFIG: &str = "\
Examples:

    agent-lens config schema          # the keys `agent-lens.toml` accepts
    agent-lens run <profile>          # execute a profile from that file
";

pub const HELP: &str = "\
Examples:

    agent-lens help          # the same reference `--help` prints
    agent-lens help --md     # every command and flag as one Markdown doc
";

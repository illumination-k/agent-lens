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

    where should I refactor first?        analyze hotspot
    is this function too complex?         analyze complexity
    did I already write this?             analyze similarity
    is this body just a forwarder?        analyze wrapper
    does this type do too many things?    analyze cohesion
    which modules are entangled?          analyze coupling
    which functions recurse mutually?     analyze cycles
    how much must I read to edit this?    analyze context-span
    which functions are load-bearing?     analyze hubs
    what breaks if I change this?         analyze impact
    which level does this code sit on?    analyze layers
    what has no test path guarding it?    analyze untested
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
    agent-lens analyze similarity src/ --format md
    agent-lens hook setup --dry-run     # wire hooks into settings.json
    agent-lens run audit                # a profile from agent-lens.toml
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
";

pub const COUPLING: &str = "\
Examples:

    agent-lens analyze coupling src/lib.rs --format md
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

pub const UNTESTED: &str = "\
Examples:

    agent-lens analyze untested src/ --format md
    agent-lens analyze untested . --exclude 'benches/**' --top 30 --format md
";

pub const HOTSPOT: &str = "\
Examples:

    agent-lens analyze hotspot src/ --format md --top 15
    agent-lens analyze hotspot . --since 90.days.ago --format md
    agent-lens analyze hotspot . --exclude-tests --exclude '*.gen.ts'
";

pub const SIMILARITY: &str = "\
Examples:

    agent-lens analyze similarity src/ --format md
    agent-lens analyze similarity src/ --sweep 0.6,0.75,0.85 --format md
    agent-lens analyze similarity . --diff-only --format md
    agent-lens analyze similarity src/ --doc-overlap --format md
    agent-lens analyze similarity src/ --method token --min-lines 10
";

pub const WRAPPER: &str = "\
Examples:

    agent-lens analyze wrapper src/ --format md
    agent-lens analyze wrapper . --diff-only --format md
";

pub const RUN: &str = "\
Run `agent-lens config schema` for the keys a profile accepts.

Examples:

    agent-lens run audit
    agent-lens run audit --config ./agent-lens.toml
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

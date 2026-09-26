//! The clap surface: every argument struct and subcommand enum the
//! `agent-lens` CLI parses.

use std::path::PathBuf;

use agent_lens::analyze::OutputFormat;
use agent_lens::skills;
use clap::{Args, Parser, Subcommand};

use super::examples;

mod analyze;
mod hooks;

pub(super) use analyze::*;
pub(super) use hooks::*;

#[derive(Debug, Parser)]
#[command(
    name = "agent-lens",
    about = "Hook handlers and analyzers that give coding agents a sharper view of the codebase.",
    after_long_help = examples::ROOT,
    version,
    propagate_version = true,
    // We ship our own `help` subcommand (with `--md`), so turn off clap's
    // auto-generated one to avoid a name clash. `--help` flags are
    // untouched and still work everywhere.
    disable_help_subcommand = true
)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Run a handler for one of Claude Code's hook events.
    #[command(subcommand)]
    Hook(HookCommand),
    /// Run a handler for one of Codex's hook events.
    #[command(subcommand)]
    CodexHook(CodexHookCommand),
    /// Run an on-demand analyzer that emits LLM-friendly context.
    #[command(subcommand, after_long_help = examples::ANALYZE)]
    Analyze(AnalyzeCommand),
    /// Run every analyzer in a named `agent-lens.toml` profile.
    ///
    /// A profile bundles a target path, shared path filters, an ordered
    /// list of analyzers, and optional per-tool overrides. The config is
    /// discovered by walking up from the current directory (or pointed
    /// at with `--config`); each analyzer runs through the same path as
    /// `agent-lens analyze`, and the per-tool reports are emitted as one
    /// combined document.
    #[command(after_long_help = examples::RUN)]
    Run(RunArgs),
    /// Snapshot a profile's analyzers as a compact set of metrics.
    ///
    /// A baseline is what turns an analyzer into a check: comparing a
    /// later run against a stored snapshot separates "this change made
    /// things worse" from "this file was already like that", so a
    /// repository can adopt a threshold without first paying off its
    /// existing debt.
    #[command(subcommand, after_long_help = examples::BASELINE)]
    Baseline(BaselineCommand),
    /// List or install the Claude Code skills bundled with this binary.
    ///
    /// The skills teach a coding agent which analyzer fits a given
    /// question, so installing them into a project's `.claude/skills`
    /// (or `$HOME/.claude/skills`) is how a fresh checkout gets
    /// `agent-lens`-aware routing.
    #[command(subcommand, after_long_help = examples::SKILLS)]
    Skills(SkillsCommand),
    /// Inspect the `agent-lens.toml` configuration format.
    #[command(subcommand, after_long_help = examples::CONFIG)]
    Config(ConfigCommand),
    /// Print the command reference, optionally as agent-friendly Markdown.
    ///
    /// Without flags this prints the same long help clap renders for
    /// `--help`. With `--md` it emits a dense Markdown document covering
    /// every subcommand, its description, and its options in one place —
    /// tuned for dropping into an LLM context.
    #[command(after_long_help = examples::HELP)]
    Help(HelpArgs),
}

#[derive(Debug, Args)]
pub(super) struct HelpArgs {
    /// Emit the full command reference as Markdown tuned for agent context.
    #[arg(long)]
    pub(super) md: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum ConfigCommand {
    /// Print the `agent-lens.toml` schema as agent-friendly Markdown.
    ///
    /// Lists the `[profile.<name>]` keys and every per-tool override
    /// table — their types, defaults, and meaning — plus a worked
    /// example. The format lives only in the config structs, so this is
    /// the canonical reference for writing or auditing an
    /// `agent-lens.toml` without reading the source.
    Schema,
}

#[derive(Debug, Subcommand)]
pub(super) enum SkillsCommand {
    /// List the bundled skills and what each one is for.
    List,
    /// Install the bundled skills into a `.claude/skills` directory.
    ///
    /// Conservative by default: a skill that already exists with
    /// different content is reported as a conflict and left untouched.
    /// Re-running once installed is a no-op; pass `--force` to overwrite
    /// local edits.
    Install(SkillsInstallArgs),
}

#[derive(Debug, Args)]
pub(super) struct SkillsInstallArgs {
    /// Where to install the bundled skills. `project` is the current
    /// directory.
    #[arg(long, value_enum, default_value_t = skills::SkillsScope::Project)]
    pub(super) scope: skills::SkillsScope,
    /// Show what would be written without touching disk.
    #[arg(long)]
    pub(super) dry_run: bool,
    /// Overwrite skills that already exist on disk with different content.
    #[arg(long)]
    pub(super) force: bool,
}

/// How a command names the profile it works on. Shared by every
/// profile-driven command so `run` and `baseline create` cannot drift on
/// what "which profile, from which config" means.
#[derive(Debug, Args)]
pub(super) struct ProfileSelectorArgs {
    /// Name of the `[profile.<name>]` table to run.
    pub(super) profile: String,
    /// Path to an explicit `agent-lens.toml`. Defaults to every one
    /// found by walking up from the current directory, taking the
    /// profile from the nearest that defines it.
    #[arg(long, value_name = "PATH")]
    pub(super) config: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct RunArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// Override the profile's `format` for this run. The profile picks
    /// the format its readers usually want; this is for the run that
    /// wants the other one — piping a normally-markdown profile into
    /// `jq`, or reading a JSON profile by eye — without editing the
    /// config.
    #[arg(long, value_enum, value_name = "FORMAT")]
    pub(super) format: Option<OutputFormat>,
    /// Replace the stacked per-tool sections with an entity-joined
    /// digest: one row per file aggregating every analyzer's findings
    /// about it, ranked by cross-tool weight, plus one line per
    /// corpus-shaped result (module cycles, modularity, co-change
    /// pairs). Rows carry no inline evidence — each names the `agent-lens
    /// analyze …` command that reproduces the full detail — so the
    /// report stays a small fraction of the full sections' size. A flag
    /// rather than a profile key on purpose: the same profile stays
    /// comparable across formats.
    #[arg(long, conflicts_with = "format")]
    pub(super) digest: bool,
}

#[derive(Debug, Subcommand)]
pub(super) enum BaselineCommand {
    /// Snapshot a profile's metrics as a JSON document.
    ///
    /// Every analyzer in the profile runs as JSON — whatever the
    /// profile's `format` says, since that key shapes the report a
    /// human or agent reads while a snapshot is built from structured
    /// fields — and each report is reduced to a handful of named
    /// numbers. Analyzers with no baseline summary yet are listed under
    /// `skipped` instead of being silently dropped.
    ///
    /// The document is deterministic: the same tree at the same commit
    /// snapshots byte-identically, with no wall-clock timestamp to make
    /// a regeneration look like a change.
    Create(BaselineCreateArgs),
    /// Compare a fresh run against a stored snapshot, and fail on a
    /// regression.
    ///
    /// The profile runs exactly as `baseline create` runs it, and each
    /// metric is judged by its own direction: extremes and totals are
    /// worse when they rise, `maintainability_index_min` is worse when
    /// it falls, and the figures that only size the measured surface
    /// (file/function/unit/module counts, `loc_total`, `edge_count`) or
    /// that track git history (`commits_max`, `score_max`) are reported
    /// when they move but never gate — a growing codebase and an extra
    /// commit are not regressions.
    ///
    /// Exits 0 when nothing gated moved the wrong way and 2 when
    /// something did, which is distinct from the 1 a failure to run
    /// exits with. `--update` turns the snapshot into a ratchet:
    /// improvements are written back, regressions keep the stored value,
    /// so the bar only ever tightens.
    Compare(BaselineCompareArgs),
}

#[derive(Debug, Args)]
pub(super) struct BaselineCreateArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// Write the snapshot here instead of stdout, creating the
    /// directory if needed.
    #[arg(long, value_name = "PATH")]
    pub(super) out: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(super) struct BaselineCompareArgs {
    #[command(flatten)]
    pub(super) selector: ProfileSelectorArgs,
    /// The stored snapshot to compare this run against — the file
    /// `baseline create --out` wrote. `--update` rewrites this same path.
    #[arg(value_name = "SNAPSHOT")]
    pub(super) snapshot: PathBuf,
    /// Output format. Defaults to JSON, which carries every metric;
    /// `md` leads with what moved.
    #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
    pub(super) format: OutputFormat,
    /// Tighten the snapshot in place: metrics this run improved are
    /// written back, metrics it regressed keep their stored value, and
    /// the exit status is unchanged. The bar only ever moves down.
    #[arg(long)]
    pub(super) update: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_lens::analyze::{
        DEFAULT_SIMILARITY_DRIFT_FLOOR, GraphDirection, GraphQueryKind, PairKey, SimilarityMethod,
        UnreachableTier,
    };
    use agent_lens::hooks::setup_engine::{HookSelection, SetupScope};
    use clap::CommandFactory;
    use rstest::rstest;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    fn help_for(args: &[&str]) -> String {
        let mut argv = args.to_vec();
        argv.push("--help");
        let err = Cli::try_parse_from(argv).expect_err("help exits before parsing");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        err.to_string()
    }

    #[test]
    fn cohesion_hook_help_describes_all_supported_unit_kinds() {
        let help = help_for(&["agent-lens", "hook", "pre-tool-use", "cohesion"]);
        assert!(help.contains("cohesion units"), "got: {help}");
        assert!(help.contains("classes"), "got: {help}");
        assert!(help.contains("module units"), "got: {help}");
        assert!(help.contains("Python, or Go"), "got: {help}");
    }

    #[test]
    fn similarity_help_does_not_mention_retired_tool_name() {
        let help = help_for(&["agent-lens", "analyze", "similarity"]);
        assert!(!help.contains("similarity-ts"), "got: {help}");
        assert!(help.contains("keeps trivial getters"), "got: {help}");
    }

    #[test]
    fn context_span_help_lists_non_rust_entry_shapes() {
        let help = help_for(&["agent-lens", "analyze", "context-span"]);
        assert!(
            help.contains("TypeScript/JavaScript entry file"),
            "got: {help}"
        );
        assert!(help.contains("Python file/directory"), "got: {help}");
        assert!(help.contains("Go file or module directory"), "got: {help}");
        assert!(help.contains("--entry-glob"), "got: {help}");
    }

    #[rstest]
    #[case::hook_session_start_summary(
        &["agent-lens", "hook", "session-start", "summary"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::SessionStart(SessionStartCommand::Summary))),
    )]
    #[case::hook_pre_tool_use_complexity(
        &["agent-lens", "hook", "pre-tool-use", "complexity"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Complexity))),
    )]
    #[case::hook_pre_tool_use_cohesion(
        &["agent-lens", "hook", "pre-tool-use", "cohesion"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PreToolUse(PreToolUseCommand::Cohesion))),
    )]
    #[case::hook_post_tool_use_similarity(
        &["agent-lens", "hook", "post-tool-use", "similarity"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Similarity))),
    )]
    #[case::hook_post_tool_use_wrapper(
        &["agent-lens", "hook", "post-tool-use", "wrapper"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Wrapper))),
    )]
    #[case::codex_hook_post_tool_use_similarity(
        &["agent-lens", "codex-hook", "post-tool-use", "similarity"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PostToolUse(CodexPostToolUseCommand::Similarity)),
        ),
    )]
    #[case::codex_hook_pre_tool_use_complexity(
        &["agent-lens", "codex-hook", "pre-tool-use", "complexity"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PreToolUse(CodexPreToolUseCommand::Complexity)),
        ),
    )]
    #[case::codex_hook_pre_tool_use_cohesion(
        &["agent-lens", "codex-hook", "pre-tool-use", "cohesion"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PreToolUse(CodexPreToolUseCommand::Cohesion)),
        ),
    )]
    #[case::codex_hook_session_start_summary(
        &["agent-lens", "codex-hook", "session-start", "summary"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::SessionStart(CodexSessionStartCommand::Summary)),
        ),
    )]
    #[case::hook_session_start_snapshot(
        &["agent-lens", "hook", "session-start", "snapshot"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::SessionStart(SessionStartCommand::Snapshot))),
    )]
    #[case::hook_post_tool_use_footprint(
        &["agent-lens", "hook", "post-tool-use", "footprint"],
        |c: &Command| matches!(c, Command::Hook(HookCommand::PostToolUse(PostToolUseCommand::Footprint))),
    )]
    #[case::hook_stop_delta(
        &["agent-lens", "hook", "stop", "delta"],
        |c: &Command| matches!(
            c,
            Command::Hook(HookCommand::Stop(StopCommand::Delta(DeltaArgs { no_block: false }))),
        ),
    )]
    #[case::hook_subagent_stop_delta_no_block(
        &["agent-lens", "hook", "subagent-stop", "delta", "--no-block"],
        |c: &Command| matches!(
            c,
            Command::Hook(HookCommand::SubagentStop(StopCommand::Delta(DeltaArgs { no_block: true }))),
        ),
    )]
    #[case::codex_hook_session_start_snapshot(
        &["agent-lens", "codex-hook", "session-start", "snapshot"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::SessionStart(CodexSessionStartCommand::Snapshot)),
        ),
    )]
    #[case::codex_hook_post_tool_use_footprint(
        &["agent-lens", "codex-hook", "post-tool-use", "footprint"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::PostToolUse(CodexPostToolUseCommand::Footprint)),
        ),
    )]
    #[case::codex_hook_stop_delta(
        &["agent-lens", "codex-hook", "stop", "delta"],
        |c: &Command| matches!(
            c,
            Command::CodexHook(CodexHookCommand::Stop(StopCommand::Delta(DeltaArgs { no_block: false }))),
        ),
    )]
    fn parses_hook_subcommand(#[case] argv: &[&str], #[case] expected: fn(&Command) -> bool) {
        let cli = Cli::try_parse_from(argv).expect("clean parse");
        assert!(
            expected(&cli.command),
            "unexpected command: {:?}",
            cli.command
        );
    }

    #[test]
    fn parses_hook_setup_with_default_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "hook", "setup"]).expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert_eq!(args.scope, SetupScope::Project);
        assert!(!args.dry_run);
    }

    #[test]
    fn parses_hook_setup_with_user_scope_and_dry_run() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "hook",
            "setup",
            "--scope",
            "user",
            "--dry-run",
        ])
        .expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert_eq!(args.scope, SetupScope::User);
        assert!(args.dry_run);
    }

    #[test]
    fn parses_hook_setup_selection_repeated_and_comma_separated() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "hook",
            "setup",
            "--only",
            "pre-tool-use,post-tool-use:wrapper",
            "--only",
            "stop",
            "--skip",
            "pre-tool-use:cohesion",
        ])
        .expect("clean parse");
        let Command::Hook(HookCommand::Setup(args)) = cli.command else {
            panic!("expected hook setup");
        };
        assert_eq!(
            args.selection.into_selection(),
            HookSelection {
                only: vec![
                    "pre-tool-use".into(),
                    "post-tool-use:wrapper".into(),
                    "stop".into()
                ],
                skip: vec!["pre-tool-use:cohesion".into()],
            },
        );
    }

    #[test]
    fn parses_codex_hook_setup_defaults_to_user_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "codex-hook", "setup"]).expect("clean parse");
        let Command::CodexHook(CodexHookCommand::Setup(args)) = cli.command else {
            panic!("expected codex-hook setup");
        };
        assert_eq!(args.scope, SetupScope::User);
        assert!(!args.dry_run);
        assert_eq!(args.selection.into_selection(), HookSelection::default());
    }

    #[test]
    fn parses_analyze_similarity_with_threshold() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--threshold",
            "0.85",
            "--format",
            "md",
            "--diff-only",
            "--exclude-tests",
            "--exclude",
            "generated/**",
            "--min-lines",
            "8",
            "--top",
            "3",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.common.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.opts.diff_only);
        assert!(args.common.path_filter.exclude_tests);
        assert_eq!(args.common.path_filter.exclude, ["generated/**"]);
        assert!((args.opts.threshold - 0.85).abs() < f64::EPSILON);
        assert_eq!(args.opts.min_lines, Some(8));
        assert_eq!(args.opts.top, Some(3));
        // `--method` is omitted above, so it defaults to TSED.
        assert_eq!(args.opts.method, SimilarityMethod::Tsed);
        // `--doc-overlap` is omitted above; the markdown rollup is opt-in.
        assert!(!args.opts.doc_overlap);
    }

    /// Every analyzer carrying both diff flags must reject the pair,
    /// and must reject a range git would read as an option. The
    /// spellings come from one macro arm, so a regression would hit all
    /// of them at once — but the arm is instantiated per analyzer, so
    /// the check is too.
    #[rstest]
    #[case::similarity("similarity")]
    #[case::complexity("complexity")]
    #[case::cohesion("cohesion")]
    #[case::delegation("delegation")]
    #[case::wrapper("wrapper")]
    fn diff_only_and_diff_range_conflict(#[case] tool: &str) {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            tool,
            "src/lib.rs",
            "--diff-only",
            "--diff-range",
            "HEAD~1..HEAD",
        ])
        .expect_err("both diff flags must conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// Spelled with `=`, an option-shaped range gets past clap's own
    /// "that looks like a flag" check and reaches the value parser —
    /// which is the seam that keeps `git diff` from being handed an
    /// option in place of a revision. Written this way on purpose: the
    /// space-separated spelling is rejected by clap before the parser
    /// runs, so it would pass without any validation of ours.
    #[rstest]
    #[case::gated_option_shaped("complexity", "--diff-range=--output=/tmp/pwned")]
    #[case::gated_blank("complexity", "--diff-range= ")]
    #[case::diff_seeded_option_shaped("impact", "--diff-range=--output=/tmp/pwned")]
    #[case::diff_seeded_blank("impact", "--diff-range=")]
    fn option_shaped_diff_range_is_rejected(#[case] tool: &str, #[case] flag: &str) {
        let err = Cli::try_parse_from(["agent-lens", "analyze", tool, "src/lib.rs", flag])
            .expect_err("an option-shaped range must not reach git");
        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn parses_analyze_complexity_with_diff_range() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            "src/lib.rs",
            "--diff-range",
            "HEAD~1..HEAD",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Complexity(args)) = cli.command else {
            panic!("expected analyze complexity");
        };
        assert_eq!(args.opts.diff_range.as_deref(), Some("HEAD~1..HEAD"));
        assert!(!args.opts.diff_only);
        assert_eq!(
            args.opts.diff_scope(),
            agent_lens::analyze::DiffScope::Range("HEAD~1..HEAD".to_owned()),
        );
    }

    #[test]
    fn parses_analyze_similarity_doc_overlap() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--doc-overlap",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert!(args.opts.doc_overlap);
    }

    #[rstest]
    #[case::token("token", SimilarityMethod::Token)]
    #[case::pdg("pdg", SimilarityMethod::Pdg)]
    fn parses_analyze_similarity_method(#[case] flag: &str, #[case] expected: SimilarityMethod) {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--method",
            flag,
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.method, expected);
    }

    #[test]
    fn parses_analyze_similarity_sweep_ladder() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.75,0.85",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.sweep, vec![0.6, 0.75, 0.85]);
    }

    #[test]
    fn analyze_similarity_rejects_sweep_with_threshold() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.85",
            "--threshold",
            "0.7",
        ])
        .expect_err("--sweep and --threshold are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    /// `--paired-by` takes either spelling of the tight key — the issue
    /// that asked for the mode named it `name`, the value enum calls it
    /// `qualified` — plus the loose `method` key.
    #[rstest]
    #[case::qualified("qualified", PairKey::Qualified)]
    #[case::name_alias("name", PairKey::Qualified)]
    #[case::method("method", PairKey::Method)]
    fn parses_analyze_similarity_paired_by(#[case] value: &str, #[case] expected: PairKey) {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--paired-by",
            value,
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.paired_by, Some(expected));
        assert!((args.opts.drift_floor - DEFAULT_SIMILARITY_DRIFT_FLOOR).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_analyze_similarity_drift_floor() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--paired-by",
            "method",
            "--drift-floor",
            "0",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.opts.drift_floor, 0.0);
    }

    /// Without `--paired-by` there is nothing for a floor to filter, so
    /// a lone `--drift-floor` is a mistake worth reporting rather than
    /// silently ignoring.
    #[test]
    fn analyze_similarity_rejects_drift_floor_without_paired_by() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--drift-floor",
            "0.5",
        ])
        .expect_err("--drift-floor requires --paired-by");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// Sweeping annotates clusters; pairing does not cluster at all.
    #[test]
    fn analyze_similarity_rejects_paired_by_with_sweep() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--sweep",
            "0.6,0.85",
            "--paired-by",
            "name",
        ])
        .expect_err("--sweep and --paired-by are mutually exclusive");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn parses_analyze_similarity_min_score_alias() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "src/lib.rs",
            "--min-score",
            "0.91",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert!((args.opts.threshold - 0.91).abs() < f64::EPSILON);
    }

    #[test]
    fn parses_analyze_complexity_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            "src/lib.rs",
            "--top",
            "12",
            "--min-score",
            "8",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Complexity(args)) = cli.command else {
            panic!("expected analyze complexity");
        };
        assert_eq!(args.opts.top, Some(12));
        assert_eq!(args.opts.min_score, Some(8));
    }

    #[test]
    fn parses_analyze_cohesion_with_top_and_min_score() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--top",
            "7",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cohesion(args)) = cli.command else {
            panic!("expected analyze cohesion");
        };
        assert_eq!(args.opts.top, Some(7));
        assert_eq!(args.opts.min_score, Some(2));
    }

    #[test]
    fn parses_analyze_hotspot_with_since_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "hotspot",
            ".",
            "--since",
            "90.days.ago",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Hotspot(args)) = cli.command else {
            panic!("expected analyze hotspot");
        };
        assert_eq!(args.opts.since.as_deref(), Some("90.days.ago"));
        assert_eq!(args.opts.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_risk_with_since_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "risk",
            ".",
            "--since",
            "90.days.ago",
            "--top",
            "5",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Risk(args)) = cli.command else {
            panic!("expected analyze risk");
        };
        assert_eq!(args.opts.since.as_deref(), Some("90.days.ago"));
        assert_eq!(args.opts.top, Some(5));
        assert!(args.common.path_filter.exclude_tests);
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_graph_query_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "graph-query",
            ".",
            "--query",
            "path",
            "--symbol",
            "handler",
            "--to",
            "db_write",
            "--depth",
            "4",
            "--limit",
            "10",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::GraphQuery(args)) = cli.command else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.opts.query, GraphQueryKind::Path);
        assert_eq!(args.opts.symbol, "handler");
        assert_eq!(args.opts.to.as_deref(), Some("db_write"));
        assert_eq!(args.opts.depth, Some(4));
        assert_eq!(args.opts.direction, None);
        assert_eq!(args.opts.limit, Some(10));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_graph_query_direction() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "graph-query",
            ".",
            "--query",
            "neighborhood",
            "--symbol",
            "resolve",
            "--direction",
            "in",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::GraphQuery(args)) = cli.command else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.opts.query, GraphQueryKind::Neighborhood);
        assert_eq!(args.opts.direction, Some(GraphDirection::In));
    }

    #[test]
    fn analyze_graph_query_requires_query_and_symbol() {
        let err = Cli::try_parse_from(["agent-lens", "analyze", "graph-query", "."])
            .expect_err("missing required flags");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_analyze_impact_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "impact",
            ".",
            "--function",
            "Resolver::resolve",
            "--function",
            "helper",
            "--depth",
            "3",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Impact(args)) = cli.command else {
            panic!("expected analyze impact");
        };
        assert_eq!(args.opts.function, ["Resolver::resolve", "helper"]);
        assert_eq!(args.opts.depth, Some(3));
        assert_eq!(args.opts.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_impact_without_flags_as_diff_mode() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "impact", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Impact(args)) = cli.command else {
            panic!("expected analyze impact");
        };
        assert!(args.opts.function.is_empty());
        assert_eq!(args.opts.depth, None);
    }

    #[test]
    fn parses_analyze_single_use_with_thresholds() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "single-use",
            "crates",
            "--max-loc",
            "12",
            "--max-cyclomatic",
            "4",
            "--top",
            "10",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::SingleUse(args)) = cli.command else {
            panic!("expected analyze single-use");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.max_loc, Some(12));
        assert_eq!(args.opts.max_cyclomatic, Some(4));
        assert_eq!(args.opts.top, Some(10));
    }

    #[test]
    fn parses_analyze_footprint_with_its_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "footprint",
            ".",
            "--diff-range",
            "main...HEAD",
            "--depth",
            "3",
            "--top",
            "5",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Footprint(args)) = cli.command else {
            panic!("expected analyze footprint");
        };
        assert_eq!(args.opts.diff_range.as_deref(), Some("main...HEAD"));
        assert_eq!(args.opts.depth, Some(3));
        assert_eq!(args.opts.top, Some(5));
        assert!(
            Cli::try_parse_from([
                "agent-lens",
                "analyze",
                "footprint",
                ".",
                "--diff-only",
                "--diff-range",
                "HEAD~1..HEAD",
            ])
            .is_err(),
            "the two diff flags conflict",
        );
    }

    #[test]
    fn parses_analyze_test_redundancy_with_its_cuts() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "test-redundancy",
            "crates",
            "--threshold",
            "0.9",
            "--method",
            "pdg",
            "--min-body-nodes",
            "12",
            "--no-reach-guard",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::TestRedundancy(args)) = cli.command else {
            panic!("expected analyze test-redundancy");
        };
        assert_eq!(args.opts.threshold, 0.9);
        assert_eq!(args.opts.method, SimilarityMethod::Pdg);
        assert_eq!(args.opts.min_body_nodes, Some(12));
        assert!(args.opts.no_reach_guard);
    }

    /// The defaults the analyzer documents are the defaults clap parses.
    #[test]
    fn analyze_test_redundancy_defaults_match_the_analyzer() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "test-redundancy", "crates"])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::TestRedundancy(args)) = cli.command else {
            panic!("expected analyze test-redundancy");
        };
        assert_eq!(
            args.opts.threshold,
            agent_lens::analyze::test_redundancy::DEFAULT_THRESHOLD,
        );
        assert_eq!(args.opts.min_body_nodes, None);
        assert!(!args.opts.no_reach_guard);
    }

    #[test]
    fn parses_analyze_parameters_with_min_call_sites() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "parameters",
            "crates",
            "--min-call-sites",
            "3",
            "--top",
            "10",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Parameters(args)) = cli.command else {
            panic!("expected analyze parameters");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.min_call_sites, Some(3));
        assert_eq!(args.opts.top, Some(10));
    }

    #[test]
    fn parses_analyze_single_impl_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "single-impl",
            "crates",
            "--top",
            "10",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::SingleImpl(args)) = cli.command else {
            panic!("expected analyze single-impl");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(10));
    }

    #[test]
    fn parses_analyze_test_only_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "test-only",
            "crates",
            "--top",
            "10",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::TestOnly(args)) = cli.command else {
            panic!("expected analyze test-only");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(10));
    }

    #[test]
    fn parses_analyze_hubs_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "hubs",
            "crates",
            "--top",
            "10",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Hubs(args)) = cli.command else {
            panic!("expected analyze hubs");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(10));
        assert!(args.common.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_layers_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "layers",
            "crates",
            "--top",
            "8",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Layers(args)) = cli.command else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(8));
        assert!(args.common.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_layers_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "layers", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Layers(args)) = cli.command else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_unreachable_with_tier_and_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "unreachable",
            "crates",
            "--tier",
            "unknown",
            "--top",
            "12",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Unreachable(args)) = cli.command else {
            panic!("expected analyze unreachable");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.tier, Some(UnreachableTier::Unknown));
        assert_eq!(args.opts.top, Some(12));
    }

    #[test]
    fn parses_analyze_unreachable_defaults_to_json_and_no_tier() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "unreachable", "."])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Unreachable(args)) = cli.command else {
            panic!("expected analyze unreachable");
        };
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.tier, None);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_untested_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "untested",
            "crates",
            "--top",
            "30",
            "--format",
            "md",
            "--exclude",
            "benches/**",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Untested(args)) = cli.command else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_untested_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "untested", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Untested(args)) = cli.command else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_visibility_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "visibility",
            "crates",
            "--top",
            "30",
            "--format",
            "md",
            "--exclude",
            "benches/**",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Visibility(args)) = cli.command else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert_eq!(args.common.path_filter.exclude, ["benches/**"]);
    }

    #[test]
    fn parses_analyze_visibility_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "visibility", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Visibility(args)) = cli.command else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_delegation_with_top_and_diff_only() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "delegation",
            "crates",
            "--format",
            "md",
            "--top",
            "30",
            "--diff-only",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Delegation(args)) = cli.command else {
            panic!("expected analyze delegation");
        };
        assert_eq!(args.common.paths, [PathBuf::from("crates")]);
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(30));
        assert!(args.opts.diff_only);
    }

    #[test]
    fn parses_analyze_delegation_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "delegation", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Delegation(args)) = cli.command else {
            panic!("expected analyze delegation");
        };
        assert_eq!(args.common.paths, [PathBuf::from(".")]);
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
        assert!(!args.opts.diff_only);
    }

    /// The monorepo case the multi-PATH signature exists for: several
    /// trees in one invocation, with the flags still parsed as flags.
    #[test]
    fn parses_several_analyze_paths() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "similarity",
            "packages",
            "cli",
            "web/src",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Similarity(args)) = cli.command else {
            panic!("expected analyze similarity");
        };
        assert_eq!(
            args.common.paths,
            [
                PathBuf::from("packages"),
                PathBuf::from("cli"),
                PathBuf::from("web/src"),
            ],
        );
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.common.path_filter.exclude_tests);
    }

    /// A repeatable option before the paths must not swallow them.
    #[test]
    fn a_repeatable_option_does_not_absorb_the_trailing_paths() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "wrapper",
            "--exclude",
            "generated/**",
            "packages",
            "cli",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Wrapper(args)) = cli.command else {
            panic!("expected analyze wrapper");
        };
        assert_eq!(args.common.path_filter.exclude, ["generated/**"]);
        assert_eq!(
            args.common.paths,
            [PathBuf::from("packages"), PathBuf::from("cli")],
        );
    }

    /// Every analyzer needs somewhere to look: an omitted PATH is a
    /// parse error, not an implicit `.`.
    #[test]
    fn analyze_requires_at_least_one_path() {
        let err = Cli::try_parse_from(["agent-lens", "analyze", "similarity", "--format", "md"])
            .expect_err("PATH is required");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// The graph-rooted analyzers grow one module graph from one entry,
    /// so a second path is a mistake worth reporting rather than a
    /// silently ignored argument.
    #[rstest]
    #[case::coupling("coupling")]
    #[case::context_span("context-span")]
    fn graph_rooted_analyzers_take_exactly_one_path(#[case] tool: &str) {
        let err = Cli::try_parse_from(["agent-lens", "analyze", tool, "packages", "cli"])
            .expect_err("a second path is not an entry point");
        assert_eq!(err.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn parses_analyze_coupling_with_top() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "coupling",
            "src/lib.rs",
            "--format",
            "md",
            "--top",
            "15",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling(args)) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.common.path, PathBuf::from("src/lib.rs"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(15));
    }

    #[test]
    fn parses_analyze_wrapper_with_top() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "wrapper", "src", "--top", "7"])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Wrapper(args)) = cli.command else {
            panic!("expected analyze wrapper");
        };
        assert_eq!(args.opts.top, Some(7));
        assert!(!args.opts.diff_only);
    }

    #[test]
    fn parses_analyze_coupling_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "coupling", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Coupling(args)) = cli.command else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.common.path, PathBuf::from("."));
        assert_eq!(args.common.format, OutputFormat::Json);
        assert_eq!(args.opts.top, None);
    }

    #[test]
    fn parses_analyze_cycles_default_format_is_json() {
        let cli =
            Cli::try_parse_from(["agent-lens", "analyze", "cycles", "."]).expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cycles(args)) = cli.command else {
            panic!("expected analyze cycles");
        };
        assert_eq!(args.paths, [PathBuf::from(".")]);
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_cycles_with_md_format_and_exclude_tests() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cycles",
            "src",
            "--format",
            "md",
            "--exclude-tests",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::Cycles(args)) = cli.command else {
            panic!("expected analyze cycles");
        };
        assert_eq!(args.paths, [PathBuf::from("src")]);
        assert_eq!(args.format, OutputFormat::Md);
        assert!(args.path_filter.exclude_tests);
    }

    #[test]
    fn parses_analyze_function_graph_default_format_is_json() {
        let cli = Cli::try_parse_from(["agent-lens", "analyze", "function-graph", "."])
            .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::FunctionGraph(args)) = cli.command else {
            panic!("expected analyze function-graph");
        };
        assert_eq!(args.paths, [PathBuf::from(".")]);
        assert_eq!(args.format, OutputFormat::Json);
    }

    #[test]
    fn parses_analyze_function_graph_with_md_format() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "function-graph",
            "src/lib.rs",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::FunctionGraph(args)) = cli.command else {
            panic!("expected analyze function-graph");
        };
        assert_eq!(args.paths, [PathBuf::from("src/lib.rs")]);
        assert_eq!(args.format, OutputFormat::Md);
    }

    #[test]
    fn parses_analyze_context_span_with_md_format() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "context-span",
            "src/lib.rs",
            "--format",
            "md",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::ContextSpan(args)) = cli.command else {
            panic!("expected analyze context-span");
        };
        assert_eq!(args.common.path, PathBuf::from("src/lib.rs"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!(args.opts.entry_glob.is_empty());
    }

    #[test]
    fn parses_analyze_context_span_with_entry_globs() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "context-span",
            "web",
            "--entry-glob",
            "app/**/page.tsx",
            "--entry-glob",
            "app/**/route.ts",
        ])
        .expect("clean parse");
        let Command::Analyze(AnalyzeCommand::ContextSpan(args)) = cli.command else {
            panic!("expected analyze context-span");
        };
        assert_eq!(args.common.path, PathBuf::from("web"));
        assert_eq!(
            args.opts.entry_glob,
            vec!["app/**/page.tsx".to_owned(), "app/**/route.ts".to_owned()]
        );
    }

    #[test]
    fn analyze_command_requires_a_subcommand() {
        let err = Cli::try_parse_from(["agent-lens", "analyze"]).expect_err("missing subcommand");
        // clap reports this as DisplayHelpOnMissingArgumentOrSubcommand
        // because the parent command has no default behaviour without a
        // subcommand.
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand,
        );
    }

    #[test]
    fn analyze_cohesion_requires_path() {
        let err =
            Cli::try_parse_from(["agent-lens", "analyze", "cohesion"]).expect_err("missing path");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument,);
    }

    #[test]
    fn invalid_format_value_is_rejected() {
        let err = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "cohesion",
            "src/lib.rs",
            "--format",
            "yaml",
        ])
        .expect_err("yaml is not a known format");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn invalid_setup_scope_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "setup", "--scope", "global"])
            .expect_err("global is not a known scope");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidValue);
    }

    #[test]
    fn unknown_subcommand_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "lint"]).expect_err("no lint subcommand");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn unknown_post_tool_use_handler_is_rejected() {
        let err = Cli::try_parse_from(["agent-lens", "hook", "post-tool-use", "complexity"])
            .expect_err("complexity is not a hook handler");
        assert_eq!(err.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }

    #[test]
    fn version_flag_short_circuits_parsing() {
        let err = Cli::try_parse_from(["agent-lens", "--version"]).expect_err("version exits");
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
    }

    /// The three `--scope` flags used to parse into CLI-local enums that
    /// were converted to the domain ones; clap now parses the domain
    /// enums directly, so the accepted spellings come from their variant
    /// names. Pin both spellings on all three commands: renaming a
    /// variant is now a CLI-visible change.
    #[rstest]
    #[case::hook_project(&["agent-lens", "hook", "setup", "--scope", "project"])]
    #[case::hook_user(&["agent-lens", "hook", "setup", "--scope", "user"])]
    #[case::codex_project(&["agent-lens", "codex-hook", "setup", "--scope", "project"])]
    #[case::codex_user(&["agent-lens", "codex-hook", "setup", "--scope", "user"])]
    #[case::skills_project(&["agent-lens", "skills", "install", "--scope", "project"])]
    #[case::skills_user(&["agent-lens", "skills", "install", "--scope", "user"])]
    fn scope_flags_accept_project_and_user(#[case] argv: &[&str]) {
        Cli::try_parse_from(argv).expect("clean parse");
    }

    #[test]
    fn scope_flags_reject_an_unknown_value() {
        assert!(
            Cli::try_parse_from(["agent-lens", "hook", "setup", "--scope", "global"]).is_err(),
            "an unknown --scope value must not parse",
        );
    }

    #[test]
    fn parses_run_with_profile_and_config() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "run",
            "web",
            "--config",
            "cfg/agent-lens.toml",
        ])
        .expect("clean parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.selector.profile, "web");
        assert_eq!(
            args.selector.config,
            Some(PathBuf::from("cfg/agent-lens.toml")),
        );
    }

    #[test]
    fn parses_run_without_config_flag() {
        let cli = Cli::try_parse_from(["agent-lens", "run", "backend"]).expect("clean parse");
        let Command::Run(args) = cli.command else {
            panic!("expected run command");
        };
        assert_eq!(args.selector.profile, "backend");
        assert_eq!(args.selector.config, None);
    }

    #[test]
    fn run_requires_a_profile_name() {
        let err = Cli::try_parse_from(["agent-lens", "run"]).expect_err("missing profile");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_baseline_create_with_profile_config_and_out() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "baseline",
            "create",
            "web",
            "--config",
            "cfg/agent-lens.toml",
            "--out",
            "target/baseline.json",
        ])
        .expect("clean parse");
        let Command::Baseline(BaselineCommand::Create(args)) = cli.command else {
            panic!("expected baseline create command");
        };
        assert_eq!(args.selector.profile, "web");
        assert_eq!(
            args.selector.config,
            Some(PathBuf::from("cfg/agent-lens.toml")),
        );
        assert_eq!(args.out, Some(PathBuf::from("target/baseline.json")));
    }

    #[test]
    fn baseline_create_defaults_to_stdout_and_a_discovered_config() {
        let cli =
            Cli::try_parse_from(["agent-lens", "baseline", "create", "web"]).expect("clean parse");
        let Command::Baseline(BaselineCommand::Create(args)) = cli.command else {
            panic!("expected baseline create command");
        };
        assert_eq!(args.selector.config, None);
        assert_eq!(args.out, None);
    }

    #[test]
    fn baseline_create_requires_a_profile_name() {
        let err =
            Cli::try_parse_from(["agent-lens", "baseline", "create"]).expect_err("missing profile");
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn parses_help_with_and_without_md() {
        let cli = Cli::try_parse_from(["agent-lens", "help", "--md"]).expect("clean parse");
        let Command::Help(args) = cli.command else {
            panic!("expected help");
        };
        assert!(args.md);

        let cli = Cli::try_parse_from(["agent-lens", "help"]).expect("clean parse");
        let Command::Help(args) = cli.command else {
            panic!("expected help");
        };
        assert!(!args.md);
    }

    #[test]
    fn parses_skills_list() {
        let cli = Cli::try_parse_from(["agent-lens", "skills", "list"]).expect("clean parse");
        assert!(matches!(cli.command, Command::Skills(SkillsCommand::List)));
    }

    #[test]
    fn parses_skills_install_with_default_scope() {
        let cli = Cli::try_parse_from(["agent-lens", "skills", "install"]).expect("clean parse");
        let Command::Skills(SkillsCommand::Install(args)) = cli.command else {
            panic!("expected skills install");
        };
        assert!(matches!(args.scope, skills::SkillsScope::Project));
        assert!(!args.dry_run);
        assert!(!args.force);
    }

    #[test]
    fn parses_skills_install_user_scope_with_flags() {
        let cli = Cli::try_parse_from([
            "agent-lens",
            "skills",
            "install",
            "--scope",
            "user",
            "--dry-run",
            "--force",
        ])
        .expect("clean parse");
        let Command::Skills(SkillsCommand::Install(args)) = cli.command else {
            panic!("expected skills install");
        };
        assert!(matches!(args.scope, skills::SkillsScope::User));
        assert!(args.dry_run);
        assert!(args.force);
    }

    /// The routing table is hand-written, so it can drift the moment a new
    /// analyzer lands. Pin it to the actual subcommand list in both
    /// directions: every analyzer is routable, and the table never names
    /// one that no longer exists.
    #[test]
    fn routing_table_names_exactly_the_analyze_subcommands() {
        let command = Cli::command();
        let analyze = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "analyze")
            .expect("analyze subcommand");

        // Routing rows are `<question>  analyze <name>`, indented to read
        // as a code block; the surrounding prose has no such row.
        let mut routed: Vec<&str> = examples::ANALYZE
            .lines()
            .filter(|line| line.starts_with("    "))
            .filter_map(|line| line.rsplit_once(" analyze "))
            .map(|(_, name)| name.trim())
            .collect();
        routed.sort_unstable();

        let mut declared: Vec<&str> = analyze
            .get_subcommands()
            .map(|sub| sub.get_name())
            .collect();
        declared.sort_unstable();

        assert_eq!(routed, declared);
    }

    /// Each analyzer's help ends with a worked invocation of *that*
    /// analyzer — a copy-pasted example block would otherwise go unnoticed.
    #[test]
    fn every_analyze_subcommand_has_its_own_example_block() {
        let command = Cli::command();
        let analyze = command
            .get_subcommands()
            .find(|sub| sub.get_name() == "analyze")
            .expect("analyze subcommand");

        for sub in analyze.get_subcommands() {
            let epilogue = sub
                .get_after_long_help()
                .map(ToString::to_string)
                .unwrap_or_default();
            let invocation = format!("    agent-lens analyze {} ", sub.get_name());
            assert!(
                epilogue.contains(&invocation),
                "`analyze {}` help is missing an example of itself: {epilogue}",
                sub.get_name(),
            );
        }
    }

    #[test]
    fn parses_config_schema_subcommand() {
        let cli = Cli::try_parse_from(["agent-lens", "config", "schema"]).expect("clean parse");
        assert!(
            matches!(cli.command, Command::Config(ConfigCommand::Schema)),
            "unexpected command: {:?}",
            cli.command,
        );
    }
}

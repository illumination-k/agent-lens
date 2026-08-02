//! The `agent-lens analyze` dispatch: mapping parsed CLI (and profile)
//! options onto the analyzers and running them.

use std::path::Path;

use agent_lens::analyze::{
    CohesionAnalyzer, ComplexityAnalyzer, ContextSpanAnalyzer, CouplingAnalyzer, CyclesAnalyzer,
    DelegationAnalyzer, FunctionGraphAnalyzer, FunctionSelection, GraphQueryAnalyzer,
    HotspotAnalyzer, HubsAnalyzer, ImpactAnalyzer, LayersAnalyzer, OutputFormat, RiskAnalyzer,
    SimilarityAnalyzer, UnreachableAnalyzer, UntestedAnalyzer, VisibilityAnalyzer, WrapperAnalyzer,
};
use agent_lens::config::{self, ConfigError};

use super::args::{
    AnalyzeCohesionArgs, AnalyzeCommand, AnalyzeCommonArgs, AnalyzeComplexityArgs,
    AnalyzeContextSpanArgs, AnalyzeDelegationArgs, AnalyzeGraphQueryArgs, AnalyzeHotspotArgs,
    AnalyzeHubsArgs, AnalyzeImpactArgs, AnalyzeLayersArgs, AnalyzePathArgs, AnalyzeRiskArgs,
    AnalyzeSimilarityArgs, AnalyzeUnreachableArgs, AnalyzeUntestedArgs, AnalyzeVisibilityArgs,
    AnalyzeWrapperArgs,
};
use super::write_stdout_line;

pub(super) fn run_analyze(cmd: AnalyzeCommand) -> Result<(), Box<dyn std::error::Error>> {
    write_stdout_line(&cmd.run()?)
}

/// Translate one profile tool entry into the [`AnalyzeCommand`] the
/// `analyze` subcommand would build for the same options.
///
/// Each analyzer's options table *is* its clap flag group, so a present
/// table drops straight into the command with no field-by-field copy. An
/// absent table means "run with the analyzer's CLI defaults", which is
/// exactly its `Default`. `graph-query` is the one tool with no defaults
/// to fall back to (its `query` and `symbol` are required), so a missing
/// table is an error — [`config::load`] already rejects such profiles,
/// this is the seam-level guard.
pub(super) fn build_analyze_command(
    tool: config::ToolName,
    profile: &config::Profile,
    target: &Path,
    format: OutputFormat,
) -> Result<AnalyzeCommand, ConfigError> {
    let common = AnalyzeCommonArgs {
        path: target.to_path_buf(),
        format,
        path_filter: AnalyzePathArgs {
            only_tests: profile.only_tests,
            exclude_tests: profile.exclude_tests,
            exclude: profile.exclude.clone(),
        },
    };
    Ok(match tool {
        config::ToolName::Cohesion => AnalyzeCommand::Cohesion(AnalyzeCohesionArgs {
            common,
            opts: profile.cohesion.clone().unwrap_or_default(),
        }),
        config::ToolName::Complexity => AnalyzeCommand::Complexity(AnalyzeComplexityArgs {
            common,
            opts: profile.complexity.clone().unwrap_or_default(),
        }),
        config::ToolName::ContextSpan => AnalyzeCommand::ContextSpan(AnalyzeContextSpanArgs {
            common,
            opts: profile.context_span.clone().unwrap_or_default(),
        }),
        config::ToolName::Delegation => AnalyzeCommand::Delegation(AnalyzeDelegationArgs {
            common,
            opts: profile.delegation.clone().unwrap_or_default(),
        }),
        config::ToolName::Hotspot => AnalyzeCommand::Hotspot(AnalyzeHotspotArgs {
            common,
            opts: profile.hotspot.clone().unwrap_or_default(),
        }),
        config::ToolName::Hubs => AnalyzeCommand::Hubs(AnalyzeHubsArgs {
            common,
            opts: profile.hubs.clone().unwrap_or_default(),
        }),
        config::ToolName::Impact => AnalyzeCommand::Impact(AnalyzeImpactArgs {
            common,
            opts: profile.impact.clone().unwrap_or_default(),
        }),
        config::ToolName::Layers => AnalyzeCommand::Layers(AnalyzeLayersArgs {
            common,
            opts: profile.layers.clone().unwrap_or_default(),
        }),
        config::ToolName::Risk => AnalyzeCommand::Risk(AnalyzeRiskArgs {
            common,
            opts: profile.risk.clone().unwrap_or_default(),
        }),
        config::ToolName::Similarity => AnalyzeCommand::Similarity(AnalyzeSimilarityArgs {
            common,
            opts: profile.similarity.clone().unwrap_or_default(),
        }),
        config::ToolName::Unreachable => AnalyzeCommand::Unreachable(AnalyzeUnreachableArgs {
            common,
            opts: profile.unreachable.clone().unwrap_or_default(),
        }),
        config::ToolName::Untested => AnalyzeCommand::Untested(AnalyzeUntestedArgs {
            common,
            opts: profile.untested.clone().unwrap_or_default(),
        }),
        config::ToolName::Visibility => AnalyzeCommand::Visibility(AnalyzeVisibilityArgs {
            common,
            opts: profile.visibility.clone().unwrap_or_default(),
        }),
        config::ToolName::Wrapper => AnalyzeCommand::Wrapper(AnalyzeWrapperArgs {
            common,
            opts: profile.wrapper.clone().unwrap_or_default(),
        }),
        config::ToolName::GraphQuery => AnalyzeCommand::GraphQuery(AnalyzeGraphQueryArgs {
            common,
            opts: profile
                .graph_query
                .clone()
                .ok_or(ConfigError::MissingToolOptions {
                    tool: config::ToolName::GraphQuery.as_str(),
                })?,
        }),
        // Analyzers with no options beyond the shared path/format args.
        config::ToolName::Coupling => AnalyzeCommand::Coupling(common),
        config::ToolName::Cycles => AnalyzeCommand::Cycles(common),
        config::ToolName::FunctionGraph => AnalyzeCommand::FunctionGraph(common),
    })
}

trait WithAnalyzePathArgs: Sized {
    fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self;
}

macro_rules! impl_with_analyze_path_args {
    ($($analyzer:ty),+ $(,)?) => {
        $(
            impl WithAnalyzePathArgs for $analyzer {
                fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self {
                    self.with_only_tests(args.only_tests)
                        .with_exclude_tests(args.exclude_tests)
                        .with_exclude_patterns(args.exclude)
                }
            }
        )+
    };
}

impl_with_analyze_path_args!(
    CohesionAnalyzer,
    ComplexityAnalyzer,
    CouplingAnalyzer,
    CyclesAnalyzer,
    DelegationAnalyzer,
    FunctionGraphAnalyzer,
    GraphQueryAnalyzer,
    ContextSpanAnalyzer,
    HotspotAnalyzer,
    HubsAnalyzer,
    ImpactAnalyzer,
    LayersAnalyzer,
    RiskAnalyzer,
    UnreachableAnalyzer,
    UntestedAnalyzer,
    VisibilityAnalyzer,
    WrapperAnalyzer,
);

// Similarity needs the same `(only_tests, exclude_tests)` args at two
// granularities: the path-level filter (skip whole files) plus a
// function-level [`FunctionSelection`] (drop `#[test]` fns inside
// non-test files). Wire both from the same args here so the analyzer
// itself never has to read the bools back out of the path filter.
impl WithAnalyzePathArgs for SimilarityAnalyzer {
    fn with_analyze_path_args(self, args: AnalyzePathArgs) -> Self {
        let selection = FunctionSelection::from_args(args.only_tests, args.exclude_tests);
        self.with_only_tests(args.only_tests)
            .with_exclude_tests(args.exclude_tests)
            .with_exclude_patterns(args.exclude)
            .with_function_selection(selection)
    }
}

impl AnalyzeCommand {
    /// Pick the right analyzer for this CLI variant and produce its
    /// report. Every arm has the same shape: hand the analyzer its whole
    /// options group, then the shared path filter. Which flags exist and
    /// what they mean lives with the analyzer, not here.
    pub(super) fn run(self) -> Result<String, Box<dyn std::error::Error>> {
        Ok(match self {
            Self::Cohesion(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                CohesionAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Complexity(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ComplexityAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::ContextSpan(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ContextSpanAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Delegation(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                DelegationAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Hotspot(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                HotspotAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Hubs(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                HubsAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Impact(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                ImpactAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Layers(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                LayersAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Risk(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                RiskAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Similarity(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                SimilarityAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Unreachable(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                UnreachableAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Untested(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                UntestedAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Visibility(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                VisibilityAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Wrapper(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                WrapperAnalyzer::new()
                    .with_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::GraphQuery(args) => {
                let (path, format, path_filter) = args.common.into_parts();
                GraphQueryAnalyzer::from_options(args.opts)
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Coupling(args) => {
                let (path, format, path_filter) = args.into_parts();
                CouplingAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::Cycles(args) => {
                let (path, format, path_filter) = args.into_parts();
                CyclesAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
            Self::FunctionGraph(args) => {
                let (path, format, path_filter) = args.into_parts();
                FunctionGraphAnalyzer::new()
                    .with_analyze_path_args(path_filter)
                    .analyze(&path, format)?
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use agent_lens::analyze::{
        DEFAULT_SIMILARITY_DRIFT_FLOOR, DEFAULT_SIMILARITY_THRESHOLD, GraphQueryKind, PairKey,
        SimilarityMethod, UnreachableTier,
    };
    use agent_lens::test_support::write_file;
    use clap::Parser;

    use super::*;
    use crate::cli::args::{Cli, Command};

    /// `WithAnalyzePathArgs for SimilarityAnalyzer` is the only special
    /// case in the trait family — it derives a [`FunctionSelection`] in
    /// addition to the path-level filter so test-function filtering
    /// stays in lock-step with path-level filtering. Drive the trait
    /// impl end-to-end on a fixture with one `#[test]` and one
    /// production function and assert each path-args combination
    /// surfaces the right corpus.
    #[test]
    fn similarity_with_analyze_path_args_threads_function_selection() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn production(x: i32) -> i32 {
    let a = x + 1;
    let b = a * 2;
    let c = b - 3;
    let d = c + 4;
    d
}

#[cfg(test)]
mod tests {
    fn alpha() -> i32 {
        let a = 1;
        let b = 2;
        let c = 3;
        let d = 4;
        a + b + c + d
    }
}
"#,
        );

        let run = |args: AnalyzePathArgs| {
            let json = SimilarityAnalyzer::new()
                .with_threshold(0.5)
                .with_analyze_path_args(args)
                .analyze(&file, OutputFormat::Json)
                .unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            parsed["unit_count"].as_u64().unwrap()
        };

        assert_eq!(run(AnalyzePathArgs::default()), 2, "All keeps both");
        assert_eq!(
            run(AnalyzePathArgs {
                only_tests: true,
                ..AnalyzePathArgs::default()
            }),
            1,
            "OnlyTests drops the production fn"
        );
        assert_eq!(
            run(AnalyzePathArgs {
                exclude_tests: true,
                ..AnalyzePathArgs::default()
            }),
            1,
            "ExcludeTests drops the test fn"
        );
    }

    #[test]
    fn analyze_command_run_executes_analyzer_with_markdown_options() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(
            dir.path(),
            "lib.rs",
            r#"
fn quiet() {}
fn branchy(n: i32) -> i32 { if n > 0 { 1 } else { 0 } }
fn dispatch(n: i32) -> i32 {
    match n { 0 => 0, 1 => 1, 2 => 2, _ => 3 }
}
"#,
        );
        let cli = Cli::try_parse_from([
            "agent-lens",
            "analyze",
            "complexity",
            file.to_str().unwrap(),
            "--format",
            "md",
            "--top",
            "1",
            "--min-score",
            "2",
        ])
        .expect("clean parse");
        let Command::Analyze(cmd) = cli.command else {
            panic!("expected analyze command");
        };
        let out = cmd.run().unwrap();
        assert!(out.contains("Top 1 by complexity"), "got: {out}");
        assert!(out.contains("`branchy`"), "got: {out}");
        assert!(!out.contains("`dispatch`"), "got: {out}");
    }

    #[test]
    fn build_analyze_command_maps_similarity_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.7\nmin-lines = 9\ntop = 4\nmethod = \"token\"\ndoc-overlap = true\npaired-by = \"method\"\ndrift-floor = 0.5\ndiff-only = true\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Similarity,
            &profile,
            Path::new("/repo/web"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Similarity(args) = cmd else {
            panic!("expected analyze similarity");
        };
        assert_eq!(args.common.path, PathBuf::from("/repo/web"));
        assert_eq!(args.common.format, OutputFormat::Md);
        assert!((args.opts.threshold - 0.7).abs() < f64::EPSILON);
        assert_eq!(args.opts.min_lines, Some(9));
        assert_eq!(args.opts.top, Some(4));
        assert_eq!(args.opts.method, SimilarityMethod::Token);
        assert!(args.opts.doc_overlap);
        assert_eq!(args.opts.paired_by, Some(PairKey::Method));
        assert!((args.opts.drift_floor - 0.5).abs() < f64::EPSILON);
        assert!(args.opts.diff_only);
    }

    #[test]
    fn build_analyze_command_uses_similarity_defaults_without_table() {
        let profile: config::Profile =
            toml::from_str("path = \"web\"\ntools = [\"similarity\"]\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Similarity,
            &profile,
            Path::new("web"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Similarity(args) = cmd else {
            panic!("expected analyze similarity");
        };
        assert!((args.opts.threshold - DEFAULT_SIMILARITY_THRESHOLD).abs() < f64::EPSILON);
        // Absent `min-lines` stays `None`: the effective floor depends on
        // `--target`, so the analyzer resolves it rather than the seam.
        assert_eq!(args.opts.min_lines, None);
        assert_eq!(args.opts.top, None);
        assert_eq!(args.opts.method, SimilarityMethod::Tsed);
        assert!(!args.opts.doc_overlap);
        // Absent `paired-by` keeps the clustering report; the floor falls
        // back to its default so it is well-defined if pairing is turned
        // on from the command line over the same profile.
        assert_eq!(args.opts.paired_by, None);
        assert!((args.opts.drift_floor - DEFAULT_SIMILARITY_DRIFT_FLOOR).abs() < f64::EPSILON);
        assert!(!args.opts.diff_only);
    }

    #[test]
    fn build_analyze_command_maps_hubs_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"hubs\"]\n\n[hubs]\ntop = 7\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Hubs,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Hubs(args) = cmd else {
            panic!("expected analyze hubs");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(7));
    }

    #[test]
    fn build_analyze_command_maps_risk_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"risk\"]\n\n[risk]\nsince = \"30.days.ago\"\ntop = 11\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Risk,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Risk(args) = cmd else {
            panic!("expected analyze risk");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.since.as_deref(), Some("30.days.ago"));
        assert_eq!(args.opts.top, Some(11));
    }

    #[test]
    fn build_analyze_command_maps_layers_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"layers\"]\n\n[layers]\ntop = 9\n")
                .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Layers,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Layers(args) = cmd else {
            panic!("expected analyze layers");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(9));
    }

    #[test]
    fn build_analyze_command_maps_visibility_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"visibility\"]\n\n[visibility]\ntop = 9\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Visibility,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Visibility(args) = cmd else {
            panic!("expected analyze visibility");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(9));
    }

    #[test]
    fn build_analyze_command_maps_delegation_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"delegation\"]\n\n\
             [delegation]\ntop = 7\ndiff-only = true\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Delegation,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Delegation(args) = cmd else {
            panic!("expected analyze delegation");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(7));
        assert!(args.opts.diff_only);
    }

    #[test]
    fn build_analyze_command_maps_unreachable_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"unreachable\"]\n\n\
             [unreachable]\ntop = 6\ntier = \"likely\"\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Unreachable,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Unreachable(args) = cmd else {
            panic!("expected analyze unreachable");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(6));
        assert_eq!(args.opts.tier, Some(UnreachableTier::Likely));
    }

    #[test]
    fn build_analyze_command_maps_untested_options() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"untested\"]\n\n[untested]\ntop = 11\n")
                .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Untested,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Untested(args) = cmd else {
            panic!("expected analyze untested");
        };
        assert_eq!(args.common.format, OutputFormat::Md);
        assert_eq!(args.opts.top, Some(11));
    }

    #[test]
    fn build_analyze_command_maps_impact_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"impact\"]\n\n\
             [impact]\nfunction = [\"Resolver::resolve\"]\ndepth = 3\ntop = 5\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Impact,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::Impact(args) = cmd else {
            panic!("expected analyze impact");
        };
        assert_eq!(args.opts.function, ["Resolver::resolve"]);
        assert_eq!(args.opts.depth, Some(3));
        assert_eq!(args.opts.top, Some(5));
        assert_eq!(args.common.format, OutputFormat::Md);
    }

    #[test]
    fn build_analyze_command_defaults_impact_to_diff_mode() {
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"impact\"]\n").unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Impact,
            &profile,
            Path::new("crates"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Impact(args) = cmd else {
            panic!("expected analyze impact");
        };
        assert!(args.opts.function.is_empty());
        assert_eq!(args.opts.depth, None);
    }

    #[test]
    fn build_analyze_command_maps_graph_query_options() {
        let profile: config::Profile = toml::from_str(
            "path = \"crates\"\ntools = [\"graph-query\"]\n\n\
             [graph-query]\nquery = \"path\"\nsymbol = \"handler\"\nto = \"db_write\"\n\
             depth = 3\nlimit = 10\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::GraphQuery,
            &profile,
            Path::new("crates"),
            OutputFormat::Md,
        )
        .unwrap();
        let AnalyzeCommand::GraphQuery(args) = cmd else {
            panic!("expected analyze graph-query");
        };
        assert_eq!(args.opts.query, GraphQueryKind::Path);
        assert_eq!(args.opts.symbol, "handler");
        assert_eq!(args.opts.to.as_deref(), Some("db_write"));
        assert_eq!(args.opts.depth, Some(3));
        assert_eq!(args.opts.direction, None);
        assert_eq!(args.opts.limit, Some(10));
        assert_eq!(args.common.format, OutputFormat::Md);
    }

    #[test]
    fn build_analyze_command_rejects_graph_query_without_table() {
        // `toml::from_str` skips `Config::validate`, so the seam-level
        // guard in `build_analyze_command` is what stands here.
        let profile: config::Profile =
            toml::from_str("path = \"crates\"\ntools = [\"graph-query\"]\n").unwrap();
        let err = build_analyze_command(
            config::ToolName::GraphQuery,
            &profile,
            Path::new("crates"),
            OutputFormat::Json,
        )
        .unwrap_err();
        assert!(
            matches!(
                err,
                ConfigError::MissingToolOptions {
                    tool: "graph-query"
                }
            ),
            "got: {err:?}",
        );
    }

    #[test]
    fn build_analyze_command_propagates_profile_path_filters() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\nexclude = [\"gen/**\"]\nexclude-tests = true\ntools = [\"coupling\"]\n",
        )
        .unwrap();
        let cmd = build_analyze_command(
            config::ToolName::Coupling,
            &profile,
            Path::new("web"),
            OutputFormat::Json,
        )
        .unwrap();
        let AnalyzeCommand::Coupling(args) = cmd else {
            panic!("expected analyze coupling");
        };
        assert_eq!(args.path_filter.exclude, ["gen/**"]);
        assert!(args.path_filter.exclude_tests);
        assert!(!args.path_filter.only_tests);
    }
}

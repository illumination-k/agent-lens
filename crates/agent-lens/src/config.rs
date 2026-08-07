//! `agent-lens.toml` configuration: named analysis profiles.
//!
//! A profile bundles a target path, shared path filters, an ordered list
//! of analyzers to run, and optional per-tool option overrides. The `run`
//! subcommand discovers the nearest `agent-lens.toml`, resolves a named
//! profile, and fans out to the selected analyzers.
//!
//! ```toml
//! [profile.web]
//! path = "web/"
//! format = "md"
//! exclude = ["tests/**/*.ts"]
//! exclude-tests = true
//! tools = ["similarity", "complexity", "cohesion"]
//!
//! [profile.web.similarity]
//! threshold = 0.9
//! min-lines = 8
//! ```
//!
//! Keys are kebab-case because they *are* the CLI flags: each per-tool
//! table deserializes into the same type that clap parses for that
//! analyzer's flag group, so the two surfaces cannot drift apart.
//! `deny_unknown_fields` turns a typo (`entrypont`) or an option set on
//! the wrong tool into a parse error instead of a silent no-op.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::analyze::OutputFormat;

/// Per-tool option tables.
///
/// Each analyzer owns its own, next to the builder that consumes it (see
/// `analyze::options`). The same type is also that analyzer's
/// clap flag group, so a `[profile.<name>.<tool>]` table and the
/// equivalent command line produce one value with no conversion between
/// them — the keys here cannot drift from the flags they mirror because
/// they are the flags. They are re-exported so the config surface still
/// reads as one module.
pub use crate::analyze::cohesion::CohesionOptions;
pub use crate::analyze::complexity::ComplexityOptions;
pub use crate::analyze::context_span::ContextSpanOptions;
pub use crate::analyze::coupling::CouplingOptions;
pub use crate::analyze::delegation::DelegationOptions;
pub use crate::analyze::graph_query::GraphQueryOptions;
pub use crate::analyze::hotspot::HotspotOptions;
pub use crate::analyze::hubs::HubsOptions;
pub use crate::analyze::impact::ImpactOptions;
pub use crate::analyze::layers::LayersOptions;
pub use crate::analyze::risk::RiskOptions;
pub use crate::analyze::similarity::SimilarityOptions;
pub use crate::analyze::unreachable::UnreachableOptions;
pub use crate::analyze::untested::UntestedOptions;
pub use crate::analyze::visibility::VisibilityOptions;
pub use crate::analyze::wrapper::WrapperOptions;

/// File name searched for when discovering a project config.
pub const CONFIG_FILE_NAME: &str = "agent-lens.toml";

/// Root of an `agent-lens.toml`: a table of named profiles.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// `[profile.<name>]` tables, keyed by profile name.
    #[serde(rename = "profile", default)]
    pub profiles: BTreeMap<String, Profile>,
}

impl Config {
    /// Look up a profile by name, listing the known names when it is
    /// missing so a typo is easy to spot.
    pub fn profile(&self, name: &str) -> Result<&Profile, ConfigError> {
        self.profiles
            .get(name)
            .ok_or_else(|| ConfigError::UnknownProfile {
                name: name.to_owned(),
                available: self.profiles.keys().cloned().collect(),
            })
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, profile) in &self.profiles {
            if profile.only_tests && profile.exclude_tests {
                return Err(ConfigError::Invalid {
                    name: name.clone(),
                    message: "`only-tests` and `exclude-tests` are mutually exclusive".to_owned(),
                });
            }
            if profile.tools.contains(&ToolName::GraphQuery) && profile.graph_query.is_none() {
                return Err(ConfigError::Invalid {
                    name: name.clone(),
                    message: "listing `graph-query` in `tools` requires a \
                              `[profile.<name>.graph-query]` table declaring `query` and `symbol`"
                        .to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// One `[profile.<name>]` entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Profile {
    /// Target path handed to every analyzer in `tools`. A relative path
    /// is resolved against the directory holding `agent-lens.toml`.
    pub path: PathBuf,
    /// Output format for the combined report. Defaults to JSON.
    pub format: Option<OutputFormat>,
    /// Extra `--exclude` globs. Passed verbatim, so they keep the same
    /// meaning as on the CLI (matched relative to the analyzed path).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Analyze only test-like files. Mutually exclusive with `exclude-tests`.
    #[serde(default)]
    pub only_tests: bool,
    /// Drop test-like files. Mutually exclusive with `only-tests`.
    #[serde(default)]
    pub exclude_tests: bool,
    /// Analyzers to run, in order.
    pub tools: Vec<ToolName>,
    /// Per-tool overrides. `None` means the table is absent and the
    /// analyzer runs with its CLI defaults.
    #[serde(default)]
    pub similarity: Option<SimilarityOptions>,
    #[serde(default)]
    pub complexity: Option<ComplexityOptions>,
    #[serde(default)]
    pub cohesion: Option<CohesionOptions>,
    #[serde(default)]
    pub hotspot: Option<HotspotOptions>,
    #[serde(default)]
    pub risk: Option<RiskOptions>,
    #[serde(default)]
    pub hubs: Option<HubsOptions>,
    #[serde(default)]
    pub impact: Option<ImpactOptions>,
    #[serde(default)]
    pub layers: Option<LayersOptions>,
    #[serde(default)]
    pub graph_query: Option<GraphQueryOptions>,
    #[serde(default)]
    pub context_span: Option<ContextSpanOptions>,
    #[serde(default)]
    pub coupling: Option<CouplingOptions>,
    #[serde(default)]
    pub delegation: Option<DelegationOptions>,
    #[serde(default)]
    pub unreachable: Option<UnreachableOptions>,
    #[serde(default)]
    pub untested: Option<UntestedOptions>,
    #[serde(default)]
    pub visibility: Option<VisibilityOptions>,
    #[serde(default)]
    pub wrapper: Option<WrapperOptions>,
}

impl Profile {
    /// Resolve `path` against the directory that holds the config file.
    /// An absolute `path` is returned unchanged.
    pub fn resolved_path(&self, config_dir: &Path) -> PathBuf {
        if self.path.is_absolute() {
            self.path.clone()
        } else {
            config_dir.join(&self.path)
        }
    }
}

/// One of the on-demand analyzers a profile can run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolName {
    Cohesion,
    Complexity,
    Coupling,
    ContextSpan,
    Cycles,
    Delegation,
    FunctionGraph,
    GraphQuery,
    Hotspot,
    Hubs,
    Impact,
    Layers,
    Risk,
    Similarity,
    Unreachable,
    Untested,
    Visibility,
    Wrapper,
}

impl ToolName {
    /// Stable lowercase spelling, matching the `analyze` subcommand name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cohesion => "cohesion",
            Self::Complexity => "complexity",
            Self::Coupling => "coupling",
            Self::ContextSpan => "context-span",
            Self::Cycles => "cycles",
            Self::Delegation => "delegation",
            Self::FunctionGraph => "function-graph",
            Self::GraphQuery => "graph-query",
            Self::Hotspot => "hotspot",
            Self::Hubs => "hubs",
            Self::Impact => "impact",
            Self::Layers => "layers",
            Self::Risk => "risk",
            Self::Similarity => "similarity",
            Self::Unreachable => "unreachable",
            Self::Untested => "untested",
            Self::Visibility => "visibility",
            Self::Wrapper => "wrapper",
        }
    }
}

/// Walk up from `start` (inclusive) and return the first `agent-lens.toml`.
pub fn discover(start: &Path) -> Option<PathBuf> {
    start.ancestors().find_map(|dir| {
        let candidate = dir.join(CONFIG_FILE_NAME);
        candidate.is_file().then_some(candidate)
    })
}

/// Read and parse the `agent-lens.toml` at `path`.
pub fn load(path: &Path) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let config: Config = toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source: Box::new(source),
    })?;
    config.validate()?;
    Ok(config)
}

/// Failures raised while discovering, reading, or interpreting a config.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("no {CONFIG_FILE_NAME} found in {start:?} or any parent directory")]
    NotFound { start: PathBuf },
    #[error("failed to read {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse {path:?}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("unknown profile {name:?}; defined profiles: [{}]", available.join(", "))]
    UnknownProfile {
        name: String,
        available: Vec<String>,
    },
    #[error("profile {name:?} is invalid: {message}")]
    Invalid { name: String, message: String },
    /// The resolution rule is spelled out rather than implied: the
    /// mistake this message catches is almost always a path written
    /// relative to the shell's cwd instead of to the config's directory.
    #[error(
        "profile {name:?}: path {path:?} does not exist (looked in {resolved:?}; a relative profile path resolves against the directory holding {CONFIG_FILE_NAME})"
    )]
    ProfilePathNotFound {
        name: String,
        path: PathBuf,
        resolved: PathBuf,
    },
    #[error("tool `{tool}` needs a `[profile.<name>.{tool}]` options table to run")]
    MissingToolOptions { tool: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::{
        DEFAULT_SIMILARITY_DRIFT_FLOOR, DEFAULT_SIMILARITY_THRESHOLD, GraphDirection,
        GraphQueryKind, PairKey,
    };
    use crate::test_support::write_file;
    use rstest::rstest;

    /// Compare two `f64` option values without tripping `clippy::float_cmp`.
    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < f64::EPSILON
    }

    const FULL: &str = r#"
[profile.web]
path = "web/"
format = "md"
exclude = ["tests/**/*.ts"]
exclude-tests = true
tools = ["similarity", "complexity", "cohesion"]

[profile.web.similarity]
threshold = 0.9
min-lines = 8
top = 20
doc-overlap = true
diff-only = false

[profile.web.complexity]
min-score = 12
top = 20

[profile.backend]
path = "crates/"
tools = ["coupling", "hotspot"]

[profile.backend.hotspot]
since = "90.days.ago"
"#;

    #[test]
    fn parses_full_config() {
        let config: Config = toml::from_str(FULL).unwrap();

        let web = config.profile("web").unwrap();
        assert_eq!(web.path, PathBuf::from("web/"));
        assert_eq!(web.format, Some(OutputFormat::Md));
        assert_eq!(web.exclude, ["tests/**/*.ts"]);
        assert!(web.exclude_tests);
        assert!(!web.only_tests);
        assert_eq!(
            web.tools,
            [
                ToolName::Similarity,
                ToolName::Complexity,
                ToolName::Cohesion
            ],
        );

        let similarity = web.similarity.as_ref().unwrap();
        assert!(close(similarity.threshold, 0.9));
        assert_eq!(similarity.min_lines, Some(8));
        assert_eq!(similarity.top, Some(20));
        assert!(similarity.doc_overlap);
        assert!(!similarity.diff_only);

        let complexity = web.complexity.as_ref().unwrap();
        assert_eq!(complexity.min_score, Some(12));
        assert!(web.cohesion.is_none(), "no [profile.web.cohesion] table");

        let backend = config.profile("backend").unwrap();
        assert_eq!(backend.tools, [ToolName::Coupling, ToolName::Hotspot]);
        assert_eq!(backend.format, None);
        assert_eq!(
            backend.hotspot.as_ref().unwrap().since.as_deref(),
            Some("90.days.ago"),
        );
    }

    #[test]
    fn parses_similarity_sweep_ladder() {
        let config: Config = toml::from_str(
            "[profile.web]\npath = \"web/\"\ntools = [\"similarity\"]\n\n[profile.web.similarity]\nsweep = [0.6, 0.75, 0.85]\n",
        )
        .unwrap();
        let similarity = config.profile("web").unwrap().similarity.as_ref().unwrap();
        assert_eq!(similarity.sweep, [0.6, 0.75, 0.85]);
        // An absent key is the analyzer's default, not "unset": the
        // options type is the clap flag group, so a profile that omits
        // `threshold` gets exactly what the bare command line would.
        assert!(close(similarity.threshold, DEFAULT_SIMILARITY_THRESHOLD));
        // Absent `doc-overlap` is off, not "unset" — the markdown rollup
        // is opt-in from both the CLI and the config file.
        assert!(!similarity.doc_overlap);
        // Absent `paired-by` leaves the clustering report in place.
        assert_eq!(similarity.paired_by, None);
        assert!(close(
            similarity.drift_floor,
            DEFAULT_SIMILARITY_DRIFT_FLOOR
        ));
    }

    #[rstest]
    #[case::qualified("qualified", PairKey::Qualified)]
    // The CLI takes `name` as an alias for the tight key; the config
    // file has to accept the same spelling or a profile cannot express
    // what a command line can.
    #[case::name_alias("name", PairKey::Qualified)]
    #[case::method("method", PairKey::Method)]
    fn parses_similarity_paired_by(#[case] value: &str, #[case] expected: PairKey) {
        let config: Config = toml::from_str(&format!(
            "[profile.web]\npath = \"web/\"\ntools = [\"similarity\"]\n\n[profile.web.similarity]\npaired-by = \"{value}\"\ndrift-floor = 0.4\n",
        ))
        .unwrap();
        let similarity = config.profile("web").unwrap().similarity.as_ref().unwrap();
        assert_eq!(similarity.paired_by, Some(expected));
        assert!(close(similarity.drift_floor, 0.4));
    }

    #[test]
    fn unknown_profile_error_lists_defined_profiles() {
        let config: Config = toml::from_str(FULL).unwrap();
        let err = config.profile("frontend").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("frontend"), "got: {msg}");
        assert!(msg.contains("web"), "got: {msg}");
        assert!(msg.contains("backend"), "got: {msg}");
    }

    #[rstest]
    #[case::unknown_profile_key(
        "[profile.web]\npath = \"web/\"\nentrypont = \"web/\"\ntools = [\"similarity\"]\n"
    )]
    #[case::unknown_tool_option(
        "[profile.web]\npath = \"web/\"\ntools = [\"similarity\"]\n\n[profile.web.similarity]\ntreshold = 0.9\n"
    )]
    #[case::table_for_optionless_tool(
        "[profile.web]\npath = \"web/\"\ntools = [\"cycles\"]\n\n[profile.web.cycles]\ntop = 5\n"
    )]
    #[case::unknown_top_level_key("widget = true\n")]
    fn rejects_unknown_keys(#[case] toml_src: &str) {
        assert!(
            toml::from_str::<Config>(toml_src).is_err(),
            "expected a parse error for: {toml_src}",
        );
    }

    #[test]
    fn parses_impact_options() {
        let config: Config = toml::from_str(
            "[profile.blast]\npath = \"src/\"\ntools = [\"impact\"]\n\n\
             [profile.blast.impact]\nfunction = [\"resolve\", \"helper\"]\ndepth = 3\ntop = 10\n",
        )
        .unwrap();
        let opts = config.profile("blast").unwrap().impact.as_ref().unwrap();
        assert_eq!(opts.function, ["resolve", "helper"]);
        assert_eq!(opts.depth, Some(3));
        assert_eq!(opts.top, Some(10));
    }

    #[test]
    fn impact_options_default_to_diff_seeding() {
        let config: Config = toml::from_str(
            "[profile.blast]\npath = \"src/\"\ntools = [\"impact\"]\n\n[profile.blast.impact]\ndepth = 2\n",
        )
        .unwrap();
        let opts = config.profile("blast").unwrap().impact.as_ref().unwrap();
        assert!(opts.function.is_empty());
        assert_eq!(opts.top, None);
    }

    #[test]
    fn parses_graph_query_options() {
        let config: Config = toml::from_str(
            "[profile.trace]\npath = \"src/\"\ntools = [\"graph-query\"]\n\n\
             [profile.trace.graph-query]\nquery = \"neighborhood\"\nsymbol = \"resolve\"\n\
             direction = \"in\"\ndepth = 2\nlimit = 25\n",
        )
        .unwrap();
        let opts = config
            .profile("trace")
            .unwrap()
            .graph_query
            .as_ref()
            .unwrap();
        assert_eq!(opts.query, GraphQueryKind::Neighborhood);
        assert_eq!(opts.symbol, "resolve");
        assert_eq!(opts.to, None);
        assert_eq!(opts.direction, Some(GraphDirection::In));
        assert_eq!(opts.depth, Some(2));
        assert_eq!(opts.limit, Some(25));
    }

    #[test]
    fn load_rejects_graph_query_tool_without_its_table() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.trace]\npath = \"src/\"\ntools = [\"graph-query\"]\n",
        );
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got: {err:?}");
        assert!(err.to_string().contains("graph-query"), "got: {err}");
    }

    #[rstest]
    #[case::missing_path("[profile.web]\ntools = [\"similarity\"]\n")]
    #[case::missing_tools("[profile.web]\npath = \"web/\"\n")]
    #[case::unknown_tool_name("[profile.web]\npath = \"web/\"\ntools = [\"lint\"]\n")]
    #[case::graph_query_without_symbol(
        "[profile.web]\npath = \"web/\"\ntools = [\"graph-query\"]\n\n[profile.web.graph-query]\nquery = \"callers\"\n"
    )]
    #[case::graph_query_without_query(
        "[profile.web]\npath = \"web/\"\ntools = [\"graph-query\"]\n\n[profile.web.graph-query]\nsymbol = \"foo\"\n"
    )]
    fn rejects_invalid_profiles(#[case] toml_src: &str) {
        assert!(
            toml::from_str::<Config>(toml_src).is_err(),
            "expected a parse error for: {toml_src}",
        );
    }

    #[test]
    fn load_accepts_only_tests_without_exclude_tests() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.web]\npath = \"web/\"\nonly-tests = true\ntools = [\"similarity\"]\n",
        );
        let config = load(&path).unwrap();
        let web = config.profile("web").unwrap();
        assert!(web.only_tests);
        assert!(!web.exclude_tests);
    }

    #[test]
    fn load_rejects_only_tests_and_exclude_tests_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.web]\npath = \"web/\"\nonly-tests = true\nexclude-tests = true\ntools = [\"similarity\"]\n",
        );
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got: {err:?}");
    }

    #[test]
    fn resolved_path_joins_relative_against_config_dir() {
        let config: Config = toml::from_str(FULL).unwrap();
        let web = config.profile("web").unwrap();
        assert_eq!(
            web.resolved_path(Path::new("/repo")),
            PathBuf::from("/repo/web/"),
        );
    }

    #[test]
    fn resolved_path_keeps_absolute_path() {
        let config: Config =
            toml::from_str("[profile.x]\npath = \"/abs/target\"\ntools = []\n").unwrap();
        let profile = config.profile("x").unwrap();
        assert_eq!(
            profile.resolved_path(Path::new("/repo")),
            PathBuf::from("/abs/target"),
        );
    }

    #[test]
    fn discover_walks_up_to_find_config() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.x]\npath = \".\"\ntools = []\n",
        );
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        let found = discover(&nested).expect("config found by walking up");
        assert_eq!(found.file_name().unwrap(), CONFIG_FILE_NAME);
        assert_eq!(found.parent().unwrap(), dir.path());
    }

    #[test]
    fn discover_returns_none_when_no_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover(dir.path()).is_none());
    }
}

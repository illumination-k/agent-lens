//! `agent-lens.toml` configuration: named analysis profiles.
//!
//! A profile bundles a target path, shared path filters, an ordered list
//! of analyzers to run, and optional per-tool option overrides. The `run`
//! subcommand discovers every `agent-lens.toml` from the current directory
//! up, resolves a named profile from the nearest one that defines it, and
//! fans out to the selected analyzers.
//!
//! Nesting is what makes a monorepo work: a package's own
//! `packages/web/agent-lens.toml` adds or overrides profiles while every
//! profile the repository root declares stays runnable from inside the
//! package. A profile is taken whole from the file that defines it — keys
//! are not merged across files — and its `path` resolves against that
//! file's directory, so a root profile means the same thing wherever it is
//! run from.
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
//! `path` takes one target or several — `path = ["internal", "cmd"]` is
//! the profile-level spelling of the multi-`PATH` command line, and means
//! the same thing: the paths are walked into one report, so a clone or a
//! call edge spanning two of them is visible where per-tree runs cannot
//! see it.
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
pub use crate::analyze::change_entropy::ChangeEntropyOptions;
pub use crate::analyze::co_change::CoChangeOptions;
pub use crate::analyze::cohesion::CohesionOptions;
pub use crate::analyze::communities::CommunitiesOptions;
pub use crate::analyze::complexity::ComplexityOptions;
pub use crate::analyze::context_span::ContextSpanOptions;
pub use crate::analyze::coupling::CouplingOptions;
pub use crate::analyze::delegation::DelegationOptions;
pub use crate::analyze::footprint::FootprintOptions;
pub use crate::analyze::graph_query::GraphQueryOptions;
pub use crate::analyze::hotspot::HotspotOptions;
pub use crate::analyze::hubs::HubsOptions;
pub use crate::analyze::impact::ImpactOptions;
pub use crate::analyze::layers::LayersOptions;
pub use crate::analyze::narrowable::NarrowableOptions;
pub use crate::analyze::reach::ReachOptions;
pub use crate::analyze::risk::RiskOptions;
pub use crate::analyze::search::SearchOptions;
pub use crate::analyze::similarity::SimilarityOptions;
pub use crate::analyze::test_redundancy::TestRedundancyOptions;
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
            if let Some(message) = profile.problem(name) {
                return Err(ConfigError::Invalid {
                    name: name.clone(),
                    message,
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
    /// Target path, or paths, handed to every analyzer in `tools`. A
    /// relative path is resolved against the directory holding
    /// `agent-lens.toml`.
    pub path: ProfilePaths,
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
    pub search: Option<SearchOptions>,
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
    pub co_change: Option<CoChangeOptions>,
    /// `hidden-coupling` scopes the same history window with the same
    /// thresholds as `co-change`, so it shares that analyzer's option
    /// type rather than declaring a byte-identical second one.
    #[serde(default)]
    pub hidden_coupling: Option<CoChangeOptions>,
    #[serde(default)]
    pub change_entropy: Option<ChangeEntropyOptions>,
    #[serde(default)]
    pub communities: Option<CommunitiesOptions>,
    #[serde(default)]
    pub hubs: Option<HubsOptions>,
    #[serde(default)]
    pub impact: Option<ImpactOptions>,
    #[serde(default)]
    pub footprint: Option<FootprintOptions>,
    #[serde(default)]
    pub layers: Option<LayersOptions>,
    #[serde(default)]
    pub narrowable: Option<NarrowableOptions>,
    #[serde(default)]
    pub reach: Option<ReachOptions>,
    #[serde(default)]
    pub graph_query: Option<GraphQueryOptions>,
    #[serde(default)]
    pub context_span: Option<ContextSpanOptions>,
    #[serde(default)]
    pub coupling: Option<CouplingOptions>,
    #[serde(default)]
    pub delegation: Option<DelegationOptions>,
    #[serde(default)]
    pub test_redundancy: Option<TestRedundancyOptions>,
    #[serde(default)]
    pub wrapper: Option<WrapperOptions>,
}

impl Profile {
    /// Resolve every entry of `path` against the directory that holds the
    /// config file. An absolute entry is kept unchanged.
    pub fn resolved_paths(&self, config_dir: &Path) -> Vec<PathBuf> {
        self.path
            .paths()
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    config_dir.join(path)
                }
            })
            .collect()
    }

    /// Why this profile cannot be run, if it cannot.
    ///
    /// Serde catches the shape of a profile; these are the constraints it
    /// cannot express — two keys that contradict each other, a tool whose
    /// required table is missing, and a corpus wider than a listed
    /// analyzer accepts. Reported as a message so [`Config::validate`]
    /// can attach the profile's name to the error once, in one place;
    /// `name` is passed in only for the messages that spell an offending
    /// table back at the reader.
    fn problem(&self, name: &str) -> Option<String> {
        if self.only_tests && self.exclude_tests {
            return Some("`only-tests` and `exclude-tests` are mutually exclusive".to_owned());
        }
        // clap rejects `--diff-only --diff-range …` at parse time; a
        // config file has no such check, so the same combination is
        // caught here rather than resolved by a silent precedence rule.
        if let Some(tool) = self.diff_conflict_tool() {
            return Some(format!(
                "`[profile.{name}.{tool}]` sets both `diff-only` and `diff-range`; they \
                 name different diffs, so set exactly one",
            ));
        }
        if self.tools.contains(&ToolName::GraphQuery) && self.graph_query.is_none() {
            return Some(
                "listing `graph-query` in `tools` requires a \
                 `[profile.<name>.graph-query]` table declaring `query` and `symbol`"
                    .to_owned(),
            );
        }
        if self.path.paths().is_empty() {
            return Some("`path` must name at least one target".to_owned());
        }
        // Caught here rather than at the first analyzer: the profile is
        // the thing that is wrong, and saying so names the fix (split the
        // single-root tools into their own profile) instead of reporting
        // one tool's failure halfway through a run.
        let single_root = self.single_root_tools();
        if self.path.paths().len() > 1 && !single_root.is_empty() {
            return Some(format!(
                "`path` lists {} paths, but [{}] grow their report from one entry point and \
                 take a single path; give them their own profile",
                self.path.paths().len(),
                single_root.join(", "),
            ));
        }
        None
    }

    /// The first tool whose options table sets both `diff-only` and
    /// `diff-range`. Only the diff-capable analyzers have the pair.
    fn diff_conflict_tool(&self) -> Option<&'static str> {
        [
            (
                "similarity",
                self.similarity.as_ref().map(|o| o.has_diff_conflict()),
            ),
            (
                "complexity",
                self.complexity.as_ref().map(|o| o.has_diff_conflict()),
            ),
            (
                "cohesion",
                self.cohesion.as_ref().map(|o| o.has_diff_conflict()),
            ),
            (
                "delegation",
                self.delegation.as_ref().map(|o| o.has_diff_conflict()),
            ),
            (
                "wrapper",
                self.wrapper.as_ref().map(|o| o.has_diff_conflict()),
            ),
            (
                "change-entropy",
                self.change_entropy.as_ref().map(|o| o.has_diff_conflict()),
            ),
        ]
        .into_iter()
        .find(|(_, conflict)| conflict.unwrap_or(false))
        .map(|(tool, _)| tool)
    }

    /// The profile's listed tools that accept only one path, in listed
    /// order and without repeats.
    fn single_root_tools(&self) -> Vec<&'static str> {
        let mut names: Vec<&'static str> = Vec::new();
        for tool in self.tools.iter().filter(|tool| tool.is_single_root()) {
            if !names.contains(&tool.as_str()) {
                names.push(tool.as_str());
            }
        }
        names
    }
}

/// A profile's `path`: one target, or several walked into one report.
///
/// The string form is the common case and stays exactly what it was, so
/// `path = "web/"` keeps parsing and keeps meaning one root. The array
/// form is the config-file spelling of the multi-`PATH` command line —
/// `path = ["internal", "cmd"]` — and exists for the same reason: a
/// cluster or a call edge spanning two trees is only visible when both
/// are in one corpus.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ProfilePaths {
    /// `path = "web/"`
    One(PathBuf),
    /// `path = ["internal", "cmd"]`
    Many(Vec<PathBuf>),
}

impl ProfilePaths {
    /// The declared paths, in the order they were written. Empty only for
    /// an explicitly empty array, which [`Config::validate`] rejects.
    pub fn paths(&self) -> &[PathBuf] {
        match self {
            Self::One(path) => std::slice::from_ref(path),
            Self::Many(paths) => paths,
        }
    }

    /// How the target is named in a baseline snapshot and in errors: the
    /// single path verbatim, or every path comma-separated — the same
    /// spelling an analyzer report's `root` field uses.
    pub fn display(&self) -> String {
        self.paths()
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// One of the on-demand analyzers a profile can run.
///
/// Deserialized from its [`ToolName::as_str`] spelling. A tool merged
/// into another fails with the name of the analyzer and section that now
/// carry it, so a stale profile says how to fix itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(try_from = "String")]
pub enum ToolName {
    ChangeEntropy,
    CoChange,
    Cohesion,
    Communities,
    Complexity,
    Coupling,
    ContextSpan,
    Cycles,
    Delegation,
    Footprint,
    FunctionGraph,
    GraphQuery,
    HiddenCoupling,
    Hotspot,
    Hubs,
    Impact,
    Layers,
    Narrowable,
    Reach,
    Risk,
    Search,
    Similarity,
    TestRedundancy,
    Wrapper,
}

/// Analyzers that became a section of another: `(old name, analyzer,
/// section)`.
pub const MERGED_TOOLS: [(&str, &str, &str); 7] = [
    ("untested", "reach", "untested"),
    ("test-only", "reach", "test-only"),
    ("unreachable", "reach", "unreachable"),
    ("single-use", "narrowable", "single-use"),
    ("single-impl", "narrowable", "single-impl"),
    ("parameters", "narrowable", "parameters"),
    ("visibility", "narrowable", "visibility"),
];

impl TryFrom<String> for ToolName {
    type Error = String;

    fn try_from(name: String) -> Result<Self, Self::Error> {
        if let Some(tool) = Self::ALL.into_iter().find(|t| t.as_str() == name) {
            return Ok(tool);
        }
        if let Some((_, tool, section)) = MERGED_TOOLS.iter().find(|(old, ..)| *old == name) {
            return Err(format!(
                "`{name}` is now the `{section}` section of `{tool}`: list `{tool}` in `tools` \
                 (set `section = [\"{section}\"]` in its table to run only this section)"
            ));
        }
        let known: Vec<&str> = Self::ALL.iter().map(|t| t.as_str()).collect();
        Err(format!(
            "unknown tool `{name}`, expected one of: {}",
            known.join(", ")
        ))
    }
}

impl ToolName {
    /// Every analyzer, in `as_str` order.
    pub const ALL: [Self; 24] = [
        Self::ChangeEntropy,
        Self::CoChange,
        Self::Cohesion,
        Self::Communities,
        Self::Complexity,
        Self::Coupling,
        Self::ContextSpan,
        Self::Cycles,
        Self::Delegation,
        Self::Footprint,
        Self::FunctionGraph,
        Self::GraphQuery,
        Self::HiddenCoupling,
        Self::Hotspot,
        Self::Hubs,
        Self::Impact,
        Self::Layers,
        Self::Narrowable,
        Self::Reach,
        Self::Risk,
        Self::Search,
        Self::Similarity,
        Self::TestRedundancy,
        Self::Wrapper,
    ];

    /// Stable lowercase spelling, matching the `analyze` subcommand name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ChangeEntropy => "change-entropy",
            Self::CoChange => "co-change",
            Self::Cohesion => "cohesion",
            Self::Communities => "communities",
            Self::Complexity => "complexity",
            Self::Coupling => "coupling",
            Self::ContextSpan => "context-span",
            Self::Cycles => "cycles",
            Self::Delegation => "delegation",
            Self::Footprint => "footprint",
            Self::FunctionGraph => "function-graph",
            Self::GraphQuery => "graph-query",
            Self::HiddenCoupling => "hidden-coupling",
            Self::Hotspot => "hotspot",
            Self::Hubs => "hubs",
            Self::Impact => "impact",
            Self::Layers => "layers",
            Self::Narrowable => "narrowable",
            Self::Reach => "reach",
            Self::Risk => "risk",
            Self::Search => "search",
            Self::Similarity => "similarity",
            Self::TestRedundancy => "test-redundancy",
            Self::Wrapper => "wrapper",
        }
    }

    /// Whether this analyzer takes exactly one path.
    ///
    /// `coupling`, `context-span` and `communities` grow a module graph
    /// outwards from a single entry point — a crate root, a TS/JS entry
    /// file, a Go module — so two entry points are two graphs rather than
    /// a wider one, and they kept the single-`PATH` CLI signature when the
    /// rest gained `PATH...`. A profile's `path` array is bounded by the
    /// same rule.
    pub fn is_single_root(self) -> bool {
        matches!(self, Self::Coupling | Self::ContextSpan | Self::Communities)
    }
}

/// Walk up from `start` (inclusive) and return every `agent-lens.toml`,
/// nearest first. Empty when there is none.
pub fn discover_all(start: &Path) -> Vec<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(CONFIG_FILE_NAME))
        .filter(|candidate| candidate.is_file())
        .collect()
}

/// A profile together with the config file that defined it, whose
/// directory its relative `path` entries resolve against.
#[derive(Debug, Clone)]
pub struct FoundProfile {
    pub config_path: PathBuf,
    pub profile: Profile,
}

impl FoundProfile {
    /// The directory holding the defining config file.
    pub fn config_dir(&self) -> &Path {
        self.config_path.parent().unwrap_or_else(|| Path::new("."))
    }
}

/// Look `name` up in `configs`, nearest first: the first file that
/// defines it wins, so a nested config overrides an ancestor's profile of
/// the same name.
///
/// Files are loaded only as far as the lookup has to go, so a broken
/// ancestor config does not fail a profile a nearer file already defines.
/// A name no file defines lists every name the chain does.
pub fn find_profile(configs: &[PathBuf], name: &str) -> Result<FoundProfile, ConfigError> {
    let mut available = std::collections::BTreeSet::new();
    for config_path in configs {
        let config = load(config_path)?;
        if let Some(profile) = config.profiles.get(name) {
            return Ok(FoundProfile {
                config_path: config_path.clone(),
                profile: profile.clone(),
            });
        }
        available.extend(config.profiles.into_keys());
    }
    Err(ConfigError::UnknownProfile {
        name: name.to_owned(),
        available: available.into_iter().collect(),
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
    #[error(
        "tool `{tool}` takes a single path, but the profile's `path` lists {count}; move `{tool}` to a profile with one `path`"
    )]
    MultiPathTool { tool: &'static str, count: usize },
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
        assert_eq!(web.path.paths(), [PathBuf::from("web/")]);
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
    fn every_tool_name_round_trips_through_its_spelling() {
        for tool in ToolName::ALL {
            assert_eq!(ToolName::try_from(tool.as_str().to_owned()), Ok(tool));
        }
    }

    /// A merged tool's name is not just unknown: the error names the
    /// analyzer and section that carry it now.
    #[rstest]
    #[case("untested", "`untested` section of `reach`")]
    #[case("test-only", "`test-only` section of `reach`")]
    #[case("unreachable", "`unreachable` section of `reach`")]
    #[case("single-use", "`single-use` section of `narrowable`")]
    #[case("single-impl", "`single-impl` section of `narrowable`")]
    #[case("parameters", "`parameters` section of `narrowable`")]
    #[case("visibility", "`visibility` section of `narrowable`")]
    #[case("typo", "unknown tool `typo`, expected one of: change-entropy")]
    fn a_removed_tool_name_says_where_it_went(#[case] name: &str, #[case] expected: &str) {
        let err = toml::from_str::<Config>(&format!(
            "[profile.x]\npath = \"src\"\ntools = [\"{name}\"]\n"
        ))
        .unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test]
    fn parses_reach_and_narrowable_sections() {
        use crate::analyze::{NarrowableSection, ReachSection};
        let config: Config = toml::from_str(
            "[profile.x]\npath = \"src\"\ntools = [\"reach\", \"narrowable\"]\n\n\
             [profile.x.reach]\nsection = [\"test-only\"]\ntier = \"likely\"\n\n\
             [profile.x.narrowable]\nsection = [\"parameters\", \"visibility\"]\nmin-call-sites = 3\n",
        )
        .unwrap();
        let profile = config.profile("x").unwrap();
        let reach = profile.reach.as_ref().unwrap();
        assert_eq!(reach.section, [ReachSection::TestOnly]);
        let narrowable = profile.narrowable.as_ref().unwrap();
        assert_eq!(
            narrowable.section,
            [NarrowableSection::Parameters, NarrowableSection::Visibility]
        );
        assert_eq!(narrowable.min_call_sites, Some(3));
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

    /// The CLI cannot express this combination — clap rejects it — so
    /// a config file is the only way to reach it, and it must fail
    /// rather than pick a winner behind the user's back.
    #[rstest]
    #[case::similarity("similarity")]
    #[case::complexity("complexity")]
    #[case::cohesion("cohesion")]
    #[case::delegation("delegation")]
    #[case::wrapper("wrapper")]
    fn load_rejects_both_diff_flags_on_one_tool(#[case] tool: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            &format!(
                "[profile.changes]\npath = \"src/\"\ntools = [\"{tool}\"]\n\n\
                 [profile.changes.{tool}]\ndiff-only = true\ndiff-range = \"HEAD~1..HEAD\"\n",
            ),
        );
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got: {err:?}");
        assert!(err.to_string().contains(tool), "got: {err}");
    }

    /// Either flag on its own is fine, including on a tool whose gate
    /// the other cases exercise — the rejection above must be about the
    /// pair, not about `diff-range` existing.
    #[rstest]
    #[case::range_alone("diff-range = \"HEAD~1..HEAD\"")]
    #[case::diff_only_alone("diff-only = true")]
    fn load_accepts_either_diff_flag_alone(#[case] setting: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            &format!(
                "[profile.changes]\npath = \"src/\"\ntools = [\"complexity\"]\n\n\
                 [profile.changes.complexity]\n{setting}\n",
            ),
        );
        load(&path).expect("one diff flag must load cleanly");
    }

    #[rstest]
    #[case::missing_path("[profile.web]\ntools = [\"similarity\"]\n")]
    #[case::path_is_neither_string_nor_array("[profile.web]\npath = 5\ntools = [\"similarity\"]\n")]
    #[case::path_array_of_non_strings("[profile.web]\npath = [1, 2]\ntools = [\"similarity\"]\n")]
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
    fn resolved_paths_joins_relative_against_config_dir() {
        let config: Config = toml::from_str(FULL).unwrap();
        let web = config.profile("web").unwrap();
        assert_eq!(
            web.resolved_paths(Path::new("/repo")),
            [PathBuf::from("/repo/web/")],
        );
    }

    #[test]
    fn resolved_paths_keeps_absolute_path() {
        let config: Config =
            toml::from_str("[profile.x]\npath = \"/abs/target\"\ntools = []\n").unwrap();
        let profile = config.profile("x").unwrap();
        assert_eq!(
            profile.resolved_paths(Path::new("/repo")),
            [PathBuf::from("/abs/target")],
        );
    }

    /// Each entry of a multi-path profile is resolved on its own, so a
    /// mix of relative and absolute entries is legal and neither kind
    /// changes the meaning of the other.
    #[test]
    fn resolved_paths_resolves_each_entry_of_an_array_independently() {
        let config: Config =
            toml::from_str("[profile.x]\npath = [\"internal\", \"/abs/cmd\"]\ntools = []\n")
                .unwrap();
        assert_eq!(
            config
                .profile("x")
                .unwrap()
                .resolved_paths(Path::new("/repo")),
            [PathBuf::from("/repo/internal"), PathBuf::from("/abs/cmd")],
        );
    }

    /// The array form is what the CLI's `PATH...` looks like in a config,
    /// and the string form has to keep parsing beside it.
    #[rstest]
    #[case::string("path = \"internal\"", &["internal"])]
    #[case::one_element_array("path = [\"internal\"]", &["internal"])]
    #[case::several("path = [\"internal\", \"cmd\"]", &["internal", "cmd"])]
    fn path_accepts_a_string_or_an_array(#[case] path: &str, #[case] expected: &[&str]) {
        let config: Config = toml::from_str(&format!(
            "[profile.backend]\n{path}\ntools = [\"similarity\", \"reach\"]\n",
        ))
        .unwrap();
        let expected: Vec<PathBuf> = expected.iter().map(PathBuf::from).collect();
        assert_eq!(config.profile("backend").unwrap().path.paths(), expected);
    }

    #[rstest]
    #[case::one("path = \"internal\"", "internal")]
    #[case::several("path = [\"internal\", \"cmd\"]", "internal, cmd")]
    fn profile_paths_display_matches_the_analyzer_root_spelling(
        #[case] path: &str,
        #[case] expected: &str,
    ) {
        let config: Config =
            toml::from_str(&format!("[profile.backend]\n{path}\ntools = []\n")).unwrap();
        assert_eq!(config.profile("backend").unwrap().path.display(), expected);
    }

    /// A one-path profile is what `coupling` and `context-span` need, so
    /// listing them beside a single-element array is fine — only a wider
    /// corpus is the mistake.
    #[rstest]
    #[case::coupling("coupling")]
    #[case::context_span("context-span")]
    fn load_accepts_a_single_element_array_for_a_single_root_tool(#[case] tool: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            &format!("[profile.backend]\npath = [\"internal\"]\ntools = [\"{tool}\"]\n"),
        );
        assert!(load(&path).is_ok());
    }

    #[rstest]
    #[case::coupling("coupling")]
    #[case::context_span("context-span")]
    fn load_rejects_several_paths_for_a_single_root_tool(#[case] tool: &str) {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            &format!(
                "[profile.backend]\npath = [\"internal\", \"cmd\"]\ntools = [\"similarity\", \"{tool}\"]\n",
            ),
        );
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got: {err:?}");
        let msg = err.to_string();
        assert!(msg.contains(tool), "offending tool not named: {msg}");
        // The tool that *is* happy with two paths must not be blamed.
        assert!(!msg.contains("similarity"), "got: {msg}");
    }

    #[test]
    fn load_rejects_an_empty_path_array() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.backend]\npath = []\ntools = [\"similarity\"]\n",
        );
        let err = load(&path).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid { .. }), "got: {err:?}");
    }

    #[rstest]
    #[case::coupling(ToolName::Coupling, true)]
    #[case::context_span(ToolName::ContextSpan, true)]
    #[case::similarity(ToolName::Similarity, false)]
    #[case::cycles(ToolName::Cycles, false)]
    fn is_single_root_marks_only_the_graph_rooted_pair(
        #[case] tool: ToolName,
        #[case] expected: bool,
    ) {
        assert_eq!(tool.is_single_root(), expected);
    }

    #[test]
    fn discover_all_walks_up_nearest_first() {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.x]\npath = \".\"\ntools = []\n",
        );
        write_file(
            &dir.path().join("a"),
            CONFIG_FILE_NAME,
            "[profile.y]\npath = \".\"\ntools = []\n",
        );
        let nested = dir.path().join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();

        assert_eq!(
            discover_all(&nested),
            vec![
                dir.path().join("a").join(CONFIG_FILE_NAME),
                dir.path().join(CONFIG_FILE_NAME),
            ],
        );
    }

    #[test]
    fn discover_all_is_empty_when_no_config_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(discover_all(dir.path()).is_empty());
    }

    /// The monorepo layout: a root config and a package config.
    fn nested_configs() -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        write_file(
            dir.path(),
            CONFIG_FILE_NAME,
            "[profile.shared]\npath = \"crates\"\ntools = [\"complexity\"]\n\
             [profile.web]\npath = \"root-web\"\ntools = [\"complexity\"]\n",
        );
        let pkg = dir.path().join("packages/web");
        write_file(
            &pkg,
            CONFIG_FILE_NAME,
            "[profile.web]\npath = \"src\"\ntools = [\"similarity\"]\n",
        );
        let chain = discover_all(&pkg);
        (dir, chain)
    }

    #[rstest]
    // The nearer file overrides a same-named ancestor profile whole.
    #[case::override_("web", "packages/web", "src", ToolName::Similarity)]
    // An ancestor-only profile stays runnable, resolved against its own dir.
    #[case::inherited("shared", "", "crates", ToolName::Complexity)]
    fn find_profile_prefers_the_nearest_definition(
        #[case] name: &str,
        #[case] config_dir: &str,
        #[case] path: &str,
        #[case] tool: ToolName,
    ) {
        let (dir, chain) = nested_configs();
        let found = find_profile(&chain, name).unwrap();
        let expected_dir = dir.path().join(config_dir);
        assert_eq!(found.config_dir(), expected_dir);
        assert_eq!(found.profile.tools, vec![tool]);
        assert_eq!(
            found.profile.resolved_paths(found.config_dir()),
            vec![expected_dir.join(path)],
        );
    }

    #[test]
    fn find_profile_lists_every_name_in_the_chain_when_missing() {
        let (_dir, chain) = nested_configs();
        let err = find_profile(&chain, "nope").unwrap_err();
        let ConfigError::UnknownProfile { available, .. } = err else {
            panic!("expected UnknownProfile, got {err:?}");
        };
        assert_eq!(available, vec!["shared".to_owned(), "web".to_owned()]);
    }

    #[test]
    fn find_profile_stops_before_a_broken_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), CONFIG_FILE_NAME, "not = [valid");
        let pkg = dir.path().join("pkg");
        write_file(
            &pkg,
            CONFIG_FILE_NAME,
            "[profile.x]\npath = \".\"\ntools = []\n",
        );
        let chain = discover_all(&pkg);
        assert!(find_profile(&chain, "x").is_ok());
        assert!(matches!(
            find_profile(&chain, "y"),
            Err(ConfigError::Parse { .. })
        ));
    }
}

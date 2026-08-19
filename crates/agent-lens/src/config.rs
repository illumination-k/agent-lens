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
pub use crate::analyze::graph_query::GraphQueryOptions;
pub use crate::analyze::hotspot::HotspotOptions;
pub use crate::analyze::hubs::HubsOptions;
pub use crate::analyze::impact::ImpactOptions;
pub use crate::analyze::layers::LayersOptions;
pub use crate::analyze::risk::RiskOptions;
pub use crate::analyze::search::SearchOptions;
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

/// What a profile declares for one analyzer's options table.
///
/// Both questions callers ask about a `[profile.<name>.<tool>]` table
/// are answered off one exhaustive walk of the struct, so neither can
/// quietly cover fewer analyzers than the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ToolTable {
    /// The table was written at all.
    present: bool,
    /// It sets both `diff-only` and `diff-range`. They name different
    /// diffs; clap rejects the pair on the command line, and a config
    /// file has no such check, so this is what [`Config::validate`]
    /// looks at instead.
    diff_conflict: bool,
}

impl ToolTable {
    /// The analyzer takes no options table at all.
    const ABSENT: Self = Self {
        present: false,
        diff_conflict: false,
    };
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

    /// What this profile declares for `tool`'s options table.
    ///
    /// The one place `Profile`'s per-tool fields are enumerated. The
    /// `match` is exhaustive on purpose: a new [`ToolName`] variant does
    /// not compile until it is named here, which is what the two lists
    /// this replaces could not promise — both were maintained by hand
    /// against the struct, and both had fallen behind it.
    fn tool_table(&self, tool: ToolName) -> ToolTable {
        /// An options table with no diff flags to contradict each other.
        macro_rules! plain {
            ($field:expr) => {
                ToolTable {
                    present: $field.is_some(),
                    diff_conflict: false,
                }
            };
        }
        /// An options table carrying both `diff-only` and `diff-range`.
        macro_rules! diffable {
            ($field:expr) => {
                ToolTable {
                    present: $field.is_some(),
                    diff_conflict: $field.as_ref().is_some_and(|o| o.has_diff_conflict()),
                }
            };
        }
        match tool {
            ToolName::ChangeEntropy => diffable!(self.change_entropy),
            ToolName::CoChange => plain!(self.co_change),
            ToolName::Cohesion => diffable!(self.cohesion),
            ToolName::Communities => plain!(self.communities),
            ToolName::Complexity => diffable!(self.complexity),
            ToolName::Coupling => plain!(self.coupling),
            ToolName::ContextSpan => plain!(self.context_span),
            ToolName::Delegation => diffable!(self.delegation),
            ToolName::GraphQuery => plain!(self.graph_query),
            ToolName::HiddenCoupling => plain!(self.hidden_coupling),
            ToolName::Hotspot => plain!(self.hotspot),
            ToolName::Hubs => plain!(self.hubs),
            ToolName::Impact => plain!(self.impact),
            ToolName::Layers => plain!(self.layers),
            ToolName::Risk => plain!(self.risk),
            ToolName::Search => plain!(self.search),
            ToolName::Similarity => diffable!(self.similarity),
            ToolName::Unreachable => plain!(self.unreachable),
            ToolName::Untested => plain!(self.untested),
            ToolName::Visibility => plain!(self.visibility),
            ToolName::Wrapper => diffable!(self.wrapper),
            // Declaring a table for either is a parse error, so there is
            // no field to read and nothing to warn about.
            ToolName::Cycles | ToolName::FunctionGraph => ToolTable::ABSENT,
        }
    }

    /// Tools whose `[profile.<name>.<tool>]` table is set but whose name
    /// the profile's `tools` list never mentions.
    ///
    /// Their options would otherwise be read, validated, and then thrown
    /// away without a word, so `run` warns about each one.
    pub fn unused_option_tables(&self) -> Vec<ToolName> {
        ToolName::ALL
            .iter()
            .copied()
            .filter(|&tool| self.tool_table(tool).present && !self.tools.contains(&tool))
            .collect()
    }

    /// The first tool whose options table sets both `diff-only` and
    /// `diff-range`. Only the diff-capable analyzers have the pair.
    fn diff_conflict_tool(&self) -> Option<&'static str> {
        ToolName::ALL
            .iter()
            .copied()
            .find(|&tool| self.tool_table(tool).diff_conflict)
            .map(ToolName::as_str)
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

/// Declare [`ToolName`] once: the variants, the canonical list, and the
/// kebab-case spelling all expand from the same rows.
///
/// What this replaces is three hand-kept lists over the same analyzers,
/// two of which had already lost an entry. Adding an analyzer here is
/// now the whole edit — nothing downstream can be short by one, because
/// nothing downstream writes the list out again.
macro_rules! tool_names {
    ($( $variant:ident => $name:literal ),+ $(,)?) => {
        /// One of the on-demand analyzers a profile can run.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
        #[serde(rename_all = "kebab-case")]
        pub enum ToolName {
            $($variant,)+
        }

        impl ToolName {
            /// Every analyzer, in declaration order.
            ///
            /// The order is a traversal order, not a ranking: it decides
            /// which tool a "both diff flags set" error names first when
            /// two tables conflict, and the order unused-table warnings
            /// come out in. `config schema` renders its own thematic
            /// order on top of this one.
            pub const ALL: &'static [ToolName] = &[$(ToolName::$variant,)+];

            /// Stable lowercase spelling, matching the `analyze`
            /// subcommand name. Pinned against serde's `kebab-case`
            /// rename by `as_str_matches_the_serde_spelling`.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)+
                }
            }
        }
    };
}

tool_names! {
    ChangeEntropy => "change-entropy",
    CoChange => "co-change",
    Cohesion => "cohesion",
    Communities => "communities",
    Complexity => "complexity",
    Coupling => "coupling",
    ContextSpan => "context-span",
    Cycles => "cycles",
    Delegation => "delegation",
    FunctionGraph => "function-graph",
    GraphQuery => "graph-query",
    HiddenCoupling => "hidden-coupling",
    Hotspot => "hotspot",
    Hubs => "hubs",
    Impact => "impact",
    Layers => "layers",
    Risk => "risk",
    Search => "search",
    Similarity => "similarity",
    Unreachable => "unreachable",
    Untested => "untested",
    Visibility => "visibility",
    Wrapper => "wrapper",
}

impl ToolName {
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
            "[profile.backend]\n{path}\ntools = [\"similarity\", \"unreachable\"]\n",
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

    /// `as_str` and the `[profile.<name>.<tool>]` heading it is used as
    /// have to be the same string serde accepts in `tools`. The macro
    /// takes the literal on trust, so this is where the two are tied
    /// together — for every variant, without a second list to keep.
    #[test]
    fn as_str_matches_the_serde_spelling() {
        for &tool in ToolName::ALL {
            let parsed: ToolName = serde_json::from_str(&format!("{:?}", tool.as_str()))
                .unwrap_or_else(|e| {
                    panic!("`{}` is not a tool name serde knows: {e}", tool.as_str())
                });
            assert_eq!(parsed, tool);
        }
    }

    /// `ALL` is what every downstream walk iterates, so a repeat would
    /// double-report a tool and a gap would hide one.
    #[test]
    fn all_lists_each_tool_once() {
        let mut names: Vec<&str> = ToolName::ALL.iter().map(|t| t.as_str()).collect();
        let listed = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), listed, "duplicate entry in ToolName::ALL");
    }

    /// The regression this refactor exists for. `coupling` and `search`
    /// were both missing from the hand-written warning list, so setting
    /// either table without listing the tool was silently ignored —
    /// which is precisely the case the warning is for.
    ///
    /// Parameterised over every analyzer that takes a table, so the next
    /// one to be added cannot be forgotten the same way.
    #[rstest]
    fn every_option_table_is_reported_when_its_tool_is_unlisted(
        #[values(
            "change-entropy",
            "co-change",
            "cohesion",
            "communities",
            "complexity",
            "coupling",
            "context-span",
            "delegation",
            "graph-query",
            "hidden-coupling",
            "hotspot",
            "hubs",
            "impact",
            "layers",
            "risk",
            "search",
            "similarity",
            "unreachable",
            "untested",
            "visibility",
            "wrapper"
        )]
        tool: &str,
    ) {
        // `tools` lists something else entirely, so the table below can
        // only be reported as unused.
        let table = match tool {
            "graph-query" => format!("[{tool}]\nquery = \"callers\"\nsymbol = \"main\"\n"),
            "search" => format!("[{tool}]\nquery = \"parse\"\n"),
            _ => format!("[{tool}]\ntop = 5\n"),
        };
        let profile: Profile =
            toml::from_str(&format!("path = \"web\"\ntools = [\"cycles\"]\n\n{table}")).unwrap();
        assert_eq!(
            profile
                .unused_option_tables()
                .iter()
                .map(|t| t.as_str())
                .collect::<Vec<_>>(),
            [tool],
        );
    }

    #[test]
    fn unused_option_tables_is_empty_when_every_table_is_listed() {
        let profile: Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\", \"coupling\"]\n\n[similarity]\nthreshold = 0.9\n\n[coupling]\ntop = 5\n",
        )
        .unwrap();
        assert!(profile.unused_option_tables().is_empty());
    }

    /// Reported in `ToolName::ALL` order rather than the order the
    /// tables appear in the file, so the warnings come out the same way
    /// whatever order the profile was written in.
    #[test]
    fn unused_option_tables_reports_in_tool_order() {
        let profile: Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[wrapper]\ndiff-only = true\n\n[complexity]\nmin-score = 3\n\n[similarity]\nthreshold = 0.9\n",
        )
        .unwrap();
        assert_eq!(
            profile.unused_option_tables(),
            [ToolName::Complexity, ToolName::Wrapper],
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

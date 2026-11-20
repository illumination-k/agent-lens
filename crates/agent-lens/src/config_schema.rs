//! Render the `agent-lens.toml` schema as agent-friendly Markdown.
//!
//! `agent-lens config schema` exists because the config format lives only
//! in the serde structs of [`crate::config`] — there is no published JSON
//! Schema and the keys are not documented elsewhere. Rather than make an
//! agent read `config.rs`, this emits a dense, decoration-free reference:
//! the `[profile.<name>]` keys, every per-tool option table, and a worked
//! example, in the same spirit as [`crate::help_md`].
//!
//! The per-tool tables are generated from each analyzer's options struct
//! (see [`tool_command`]): the struct is both the `analyze <tool>` flag
//! group and the `[profile.<name>.<tool>]` table, so its clap metadata —
//! long name, doc comment, accepted values, default — is the schema, and
//! a new option documents itself. The `match` there is exhaustive, so a
//! new analyzer fails to compile until it names its options type. Only
//! defaults clap cannot see (resolved at run time) are written out, in
//! [`RUNTIME_DEFAULTS`]. The `schema_keys_match_struct_fields` parity test
//! still reflects each struct's serde field list, which is what the TOML
//! actually accepts, against the generated keys.

use std::any::TypeId;
use std::fmt::Write as _;

use clap::{Arg, ArgAction, Args, Command};

use crate::config::{
    CONFIG_FILE_NAME, ChangeEntropyOptions, CoChangeOptions, CohesionOptions, CommunitiesOptions,
    ComplexityOptions, ContextSpanOptions, CouplingOptions, FootprintOptions, ForwardingOptions,
    GraphQueryOptions, HotspotOptions, HubsOptions, ImpactOptions, LayersOptions,
    NarrowableOptions, OwnershipOptions, ReachOptions, RiskOptions, SearchOptions,
    SimilarityOptions, TestRedundancyOptions, ToolName,
};

/// Order the per-tool tables are rendered in. Kept in sync with the
/// exhaustive `match` in [`tool_table`]; a missing variant there is a
/// compile error, and the cohesion test guards the reverse direction.
const TOOL_ORDER: [ToolName; 24] = [
    ToolName::Search,
    ToolName::Similarity,
    ToolName::Complexity,
    ToolName::Cohesion,
    ToolName::Hotspot,
    ToolName::Risk,
    ToolName::CoChange,
    ToolName::ChangeEntropy,
    ToolName::Ownership,
    ToolName::HiddenCoupling,
    ToolName::Hubs,
    ToolName::Impact,
    ToolName::Footprint,
    ToolName::Layers,
    ToolName::Reach,
    ToolName::Narrowable,
    ToolName::TestRedundancy,
    ToolName::Forwarding,
    ToolName::GraphQuery,
    ToolName::ContextSpan,
    ToolName::Coupling,
    ToolName::Communities,
    ToolName::FunctionGraph,
    ToolName::Cycles,
];

/// One configuration key: its TOML spelling, value type, whether it is
/// required (or its default), and what it does.
struct Field {
    /// Kebab-case key as written in the TOML.
    key: &'static str,
    /// Human-readable value type, e.g. `string` or `array<string>`.
    ty: &'static str,
    /// `required`, `optional`, or a `default: …` note.
    presence: &'static str,
    /// What the key controls. Mirrors the doc comment on the analyzer's
    /// options struct, which is also that flag's `--help` text.
    desc: &'static str,
}

/// Top-level `[profile.<name>]` keys shared by every analyzer.
const PROFILE_FIELDS: &[Field] = &[
    Field {
        key: "path",
        ty: "string (path) or array<string>",
        presence: "required",
        desc: "Target path handed to every analyzer in `tools`, or an array of them walked into one report (the config spelling of `analyze <tool> PATH...`). A relative path is resolved against the directory holding agent-lens.toml. An array of more than one path is rejected for coupling, context-span and communities, which take a single entry point.",
    },
    Field {
        key: "tools",
        ty: "array<tool-name>",
        presence: "required",
        desc: "Analyzers to run, in order. Each entry is one of: change-entropy, cohesion, communities, complexity, coupling, context-span, co-change, cycles, footprint, forwarding, function-graph, graph-query, hidden-coupling, hotspot, hubs, impact, layers, narrowable, ownership, reach, risk, search, similarity, test-redundancy. The former untested, test-only and unreachable are sections of reach; single-use, single-impl, parameters and visibility are sections of narrowable; wrapper and delegation are sections of forwarding.",
    },
    Field {
        key: "format",
        ty: "\"json\" or \"md\"",
        presence: "default: json",
        desc: "Output format for the combined report.",
    },
    Field {
        key: "exclude",
        ty: "array<string> (globs)",
        presence: "default: []",
        desc: "Extra exclude globs, matched relative to the analyzed path (same meaning as the --exclude CLI flag).",
    },
    Field {
        key: "only-tests",
        ty: "bool",
        presence: "default: false",
        desc: "Analyze only test-like files. Mutually exclusive with exclude-tests.",
    },
    Field {
        key: "exclude-tests",
        ty: "bool",
        presence: "default: false",
        desc: "Drop test-like files. Mutually exclusive with only-tests.",
    },
];

/// One rendered table row. Per-tool rows are built from the options
/// structs at render time, so they own their text.
struct Row {
    key: String,
    ty: String,
    presence: String,
    desc: String,
}

impl From<&Field> for Row {
    fn from(field: &Field) -> Self {
        Self {
            key: field.key.to_owned(),
            ty: field.ty.to_owned(),
            presence: field.presence.to_owned(),
            desc: field.desc.to_owned(),
        }
    }
}

/// The clap argument surface of one analyzer's options struct, or `None`
/// for analyzers that take no per-tool overrides.
///
/// Every options struct is both the `analyze <tool>` flag group and the
/// `[profile.<name>.<tool>]` table, so its clap metadata — long name, doc
/// comment, accepted values, default — *is* the schema. This `match` is
/// exhaustive on purpose: a new [`ToolName`] variant will not compile
/// until it names its options type here.
fn tool_command(tool: ToolName) -> Option<Command> {
    fn of<T: Args>(name: &'static str) -> Option<Command> {
        Some(T::augment_args(Command::new(name)))
    }
    match tool {
        ToolName::Search => of::<SearchOptions>("search"),
        ToolName::Similarity => of::<SimilarityOptions>("similarity"),
        ToolName::Complexity => of::<ComplexityOptions>("complexity"),
        ToolName::Cohesion => of::<CohesionOptions>("cohesion"),
        ToolName::Hotspot => of::<HotspotOptions>("hotspot"),
        ToolName::Risk => of::<RiskOptions>("risk"),
        // `hidden-coupling` scopes the same history window with the same
        // thresholds as `co-change`, so they share one options type.
        ToolName::CoChange | ToolName::HiddenCoupling => of::<CoChangeOptions>("co-change"),
        ToolName::ChangeEntropy => of::<ChangeEntropyOptions>("change-entropy"),
        ToolName::Ownership => of::<OwnershipOptions>("ownership"),
        ToolName::Hubs => of::<HubsOptions>("hubs"),
        ToolName::Impact => of::<ImpactOptions>("impact"),
        ToolName::Footprint => of::<FootprintOptions>("footprint"),
        ToolName::Layers => of::<LayersOptions>("layers"),
        ToolName::Reach => of::<ReachOptions>("reach"),
        ToolName::Narrowable => of::<NarrowableOptions>("narrowable"),
        ToolName::TestRedundancy => of::<TestRedundancyOptions>("test-redundancy"),
        ToolName::Forwarding => of::<ForwardingOptions>("forwarding"),
        ToolName::GraphQuery => of::<GraphQueryOptions>("graph-query"),
        ToolName::ContextSpan => of::<ContextSpanOptions>("context-span"),
        ToolName::Coupling => of::<CouplingOptions>("coupling"),
        ToolName::Communities => of::<CommunitiesOptions>("communities"),
        // Declaring a `[profile.<name>.cycles]` table is a parse error.
        ToolName::Cycles | ToolName::FunctionGraph => None,
    }
}

/// Defaults clap cannot see: an `Option<_>` field the analyzer resolves
/// at run time, or a default that depends on another key. Everything
/// else in a row comes from the options struct; a test holds each entry
/// to a real key that has no clap default of its own.
const RUNTIME_DEFAULTS: &[(ToolName, &str, &str)] = &[
    (ToolName::Search, "limit", "20"),
    (ToolName::Search, "fuzzy", "missing"),
    (ToolName::Search, "rank", "bm25"),
    (
        ToolName::Similarity,
        "min-lines",
        "5 (functions) / 3 (types)",
    ),
    (ToolName::Ownership, "bot-pattern", "[\"*\\\\[bot\\\\]*\"]"),
    (ToolName::Impact, "depth", "5"),
    (ToolName::Footprint, "depth", "2"),
    (ToolName::Reach, "section", "all three"),
    (ToolName::Reach, "tier", "confirmed"),
    (ToolName::Narrowable, "section", "all four"),
    (ToolName::Narrowable, "max-loc", "30"),
    (ToolName::Narrowable, "max-cyclomatic", "6"),
    (ToolName::Narrowable, "min-call-sites", "2"),
    (ToolName::TestRedundancy, "min-lines", "5"),
    (ToolName::TestRedundancy, "min-body-nodes", "8"),
    (ToolName::Forwarding, "top", "20"),
    (ToolName::Forwarding, "section", "both"),
    (ToolName::GraphQuery, "depth", "1 (path: unbounded)"),
    (ToolName::GraphQuery, "direction", "both"),
    (ToolName::GraphQuery, "limit", "50"),
    (ToolName::Coupling, "top", "20"),
];

/// Per-tool rows, sorted by key, derived from the options struct.
fn tool_rows(tool: ToolName) -> Option<Vec<Row>> {
    let command = tool_command(tool)?;
    let mut rows: Vec<Row> = command
        .get_arguments()
        .filter(|arg| !arg.is_hide_set())
        .filter_map(|arg| {
            let key = arg.get_long()?;
            Some(Row {
                key: key.to_owned(),
                ty: value_type(arg),
                presence: presence(tool, key, arg),
                desc: arg
                    .get_help()
                    .map(|help| help.to_string().replace('\n', " "))
                    .unwrap_or_default(),
            })
        })
        .collect();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    Some(rows)
}

/// The TOML value type of one option.
fn value_type(arg: &Arg) -> String {
    if matches!(arg.get_action(), ArgAction::SetTrue | ArgAction::SetFalse) {
        return "bool".to_owned();
    }
    let choices: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| format!("\"{}\"", value.get_name()))
        .collect();
    let base = if choices.is_empty() {
        scalar_type(arg).to_owned()
    } else {
        english_or(&choices)
    };
    if matches!(arg.get_action(), ArgAction::Append) {
        format!("array<{base}>")
    } else {
        base
    }
}

fn scalar_type(arg: &Arg) -> &'static str {
    let id = arg.get_value_parser().type_id();
    let is = |t: TypeId| id == t;
    if is(TypeId::of::<f64>()) {
        "float"
    } else if [
        TypeId::of::<usize>(),
        TypeId::of::<u8>(),
        TypeId::of::<u16>(),
        TypeId::of::<u32>(),
        TypeId::of::<u64>(),
        TypeId::of::<i32>(),
        TypeId::of::<i64>(),
    ]
    .into_iter()
    .any(is)
    {
        "int"
    } else {
        "string"
    }
}

/// `"a"`, `"a" or "b"`, `"a", "b" or "c"`.
fn english_or(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} or {last}", init.join(", ")),
    }
}

/// `required`, `default: …`, or `optional`, in that order of precedence:
/// a clap default, then a runtime default, then what the action implies.
fn presence(tool: ToolName, key: &str, arg: &Arg) -> String {
    if arg.is_required_set() {
        return "required".to_owned();
    }
    let defaults: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if !defaults.is_empty() {
        return format!("default: {}", defaults.join(", "));
    }
    if let Some(&(_, _, value)) = RUNTIME_DEFAULTS
        .iter()
        .find(|&&(t, k, _)| t == tool && k == key)
    {
        return format!("default: {value}");
    }
    match arg.get_action() {
        ArgAction::SetTrue => "default: false".to_owned(),
        ArgAction::Append => "default: []".to_owned(),
        _ => "optional".to_owned(),
    }
}

/// A compact, valid config that exercises a profile, shared filters,
/// two per-tool override tables, and the multi-path `path` form.
const EXAMPLE: &str = "\
[profile.web]
path = \"web/\"
format = \"md\"
exclude = [\"tests/**/*.ts\"]
exclude-tests = true
tools = [\"similarity\", \"complexity\", \"cohesion\"]

[profile.web.similarity]
threshold = 0.9
min-lines = 8
top = 20

[profile.web.complexity]
min-score = 12

[profile.backend]
path = \"crates/\"
tools = [\"coupling\", \"hotspot\"]

[profile.backend.hotspot]
since = \"90.days.ago\"

# Two trees walked into one corpus: a clone or a call edge spanning them
# is invisible to a profile that can only name one of them.
[profile.services]
path = [\"internal\", \"cmd\"]
tools = [\"similarity\", \"reach\"]

[profile.services.reach]
section = [\"unreachable\"]";

/// Render the whole `agent-lens.toml` schema as one Markdown document.
pub fn render() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {CONFIG_FILE_NAME} schema");
    out.push_str(
        "\nNamed analysis profiles for the `run` subcommand. Every file \
         from the current directory up is discovered, and a profile is \
         taken whole from the nearest file that defines it, so a nested \
         config (a monorepo package's own) overrides or adds profiles \
         while its ancestors' stay runnable; `--config` names one file \
         instead. Keys are kebab-case to mirror the CLI flags, and unknown \
         keys are a parse error.\n",
    );

    let _ = writeln!(out, "\n## `[profile.<name>]`");
    out.push_str(
        "\nOne table per profile. `<name>` is the argument passed to \
         `agent-lens run <name>`.\n",
    );
    let profile_rows: Vec<Row> = PROFILE_FIELDS.iter().map(Row::from).collect();
    render_rows(&mut out, &profile_rows);

    out.push_str(
        "\n## Per-tool overrides\n\nEach `[profile.<name>.<tool>]` table is \
         optional; an absent table runs the analyzer with its CLI defaults. \
         A table for an analyzer that is not listed in `tools` is still \
         parsed but never applied.\n",
    );

    for tool in TOOL_ORDER {
        let name = tool.as_str();
        let _ = writeln!(out, "\n### `[profile.<name>.{name}]`");
        match tool_rows(tool) {
            Some(rows) => render_rows(&mut out, &rows),
            None => {
                let _ = writeln!(out, "\nNo options. Declaring this table is a parse error.");
            }
        }
    }

    out.push_str("\n## Example\n\n```toml\n");
    out.push_str(EXAMPLE);
    out.push_str("\n```\n");
    out
}

/// Render rows as a Markdown table. A literal `|` in a cell would split
/// it, so cells escape it.
fn render_rows(out: &mut String, rows: &[Row]) {
    out.push_str("\n| Key | Type | Presence | Description |\n| --- | --- | --- | --- |\n");
    let cell = |text: &str| text.replace('|', "\\|");
    for row in rows {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            row.key,
            cell(&row.ty),
            cell(&row.presence),
            cell(&row.desc),
        );
    }
}

/// Reflect the serde field names of a `#[derive(Deserialize)]` struct so a
/// parity test can compare them against the hand-written schema without a
/// second hand-written list to drift.
///
/// serde's derived `Deserialize` hands its (renamed, kebab-case) field list
/// to `Deserializer::deserialize_struct`. We capture that slice and abort,
/// so the returned names are exactly what the struct declares — no more, no
/// less.
#[cfg(test)]
mod reflect {
    use std::fmt;

    use serde::de::{self, Deserialize, Deserializer, Visitor};

    /// Sentinel error: we bail out the instant the field list is captured,
    /// so a successful deserialize never happens.
    #[derive(Debug)]
    struct Captured;

    impl fmt::Display for Captured {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("captured struct fields")
        }
    }

    impl std::error::Error for Captured {}

    impl de::Error for Captured {
        fn custom<T: fmt::Display>(_: T) -> Self {
            Captured
        }
    }

    struct Reflector<'a>(&'a mut Vec<&'static str>);

    impl<'de> Deserializer<'de> for Reflector<'_> {
        type Error = Captured;

        fn deserialize_struct<V>(
            self,
            _name: &'static str,
            fields: &'static [&'static str],
            _visitor: V,
        ) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            self.0.extend_from_slice(fields);
            Err(Captured)
        }

        fn deserialize_any<V>(self, _visitor: V) -> Result<V::Value, Self::Error>
        where
            V: Visitor<'de>,
        {
            Err(Captured)
        }

        serde::forward_to_deserialize_any! {
            bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
            bytes byte_buf option unit unit_struct newtype_struct seq tuple
            tuple_struct map enum identifier ignored_any
        }
    }

    /// The serde field names a struct declares, in schema (kebab-case) form.
    pub fn field_names<'de, T: Deserialize<'de>>() -> Vec<&'static str> {
        let mut names = Vec::new();
        let _ = T::deserialize(Reflector(&mut names));
        names
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::analyze::{DEFAULT_SIMILARITY_MIN_LINES, DEFAULT_SIMILARITY_THRESHOLD};
    use crate::config::{
        ChangeEntropyOptions, CoChangeOptions, CohesionOptions, CommunitiesOptions,
        ComplexityOptions, ContextSpanOptions, CouplingOptions, FootprintOptions,
        ForwardingOptions, GraphQueryOptions, HotspotOptions, HubsOptions, ImpactOptions,
        LayersOptions, NarrowableOptions, OwnershipOptions, Profile, ReachOptions, RiskOptions,
        SearchOptions, SimilarityOptions, TestRedundancyOptions,
    };

    /// Schema keys documented for `tool` must match, exactly, the serde field
    /// names of its options struct `T`. Catches both drift directions: a new
    /// struct field missing from the schema, and a schema key whose field was
    /// removed.
    fn assert_tool_parity<'de, T: serde::Deserialize<'de>>(tool: ToolName) {
        let rows =
            tool_rows(tool).unwrap_or_else(|| panic!("{tool:?} should have an options table"));
        let schema: BTreeSet<&str> = rows.iter().map(|r| r.key.as_str()).collect();
        let fields: BTreeSet<&str> = reflect::field_names::<T>().into_iter().collect();
        assert_eq!(schema, fields, "schema/struct field drift for {tool:?}");
    }

    #[test]
    fn schema_keys_match_struct_fields() {
        assert_tool_parity::<SearchOptions>(ToolName::Search);
        assert_tool_parity::<SimilarityOptions>(ToolName::Similarity);
        assert_tool_parity::<ComplexityOptions>(ToolName::Complexity);
        assert_tool_parity::<CohesionOptions>(ToolName::Cohesion);
        assert_tool_parity::<CoChangeOptions>(ToolName::CoChange);
        assert_tool_parity::<ChangeEntropyOptions>(ToolName::ChangeEntropy);
        assert_tool_parity::<OwnershipOptions>(ToolName::Ownership);
        assert_tool_parity::<CommunitiesOptions>(ToolName::Communities);
        // `hidden-coupling` scopes the same history window with the same
        // thresholds as `co-change`, so they share one options type —
        // and the schema has to keep describing that one type under both
        // names.
        assert_tool_parity::<CoChangeOptions>(ToolName::HiddenCoupling);
        assert_tool_parity::<HotspotOptions>(ToolName::Hotspot);
        assert_tool_parity::<RiskOptions>(ToolName::Risk);
        assert_tool_parity::<HubsOptions>(ToolName::Hubs);
        assert_tool_parity::<ImpactOptions>(ToolName::Impact);
        assert_tool_parity::<FootprintOptions>(ToolName::Footprint);
        assert_tool_parity::<LayersOptions>(ToolName::Layers);
        assert_tool_parity::<ReachOptions>(ToolName::Reach);
        assert_tool_parity::<NarrowableOptions>(ToolName::Narrowable);
        assert_tool_parity::<TestRedundancyOptions>(ToolName::TestRedundancy);
        assert_tool_parity::<ForwardingOptions>(ToolName::Forwarding);
        assert_tool_parity::<GraphQueryOptions>(ToolName::GraphQuery);
        assert_tool_parity::<ContextSpanOptions>(ToolName::ContextSpan);
        assert_tool_parity::<CouplingOptions>(ToolName::Coupling);
    }

    #[test]
    fn profile_schema_keys_match_profile_struct_fields() {
        // The `Profile` struct carries the shared keys plus one field per
        // option-bearing tool (its nested override table). The schema splits
        // those apart — shared keys in `PROFILE_FIELDS`, override tables under
        // their own headings — so reassemble the union before comparing.
        let schema: BTreeSet<&str> = PROFILE_FIELDS
            .iter()
            .map(|f| f.key)
            .chain(
                TOOL_ORDER
                    .into_iter()
                    .filter(|&t| tool_command(t).is_some())
                    .map(ToolName::as_str),
            )
            .collect();
        let fields: BTreeSet<&str> = reflect::field_names::<Profile>().into_iter().collect();
        assert_eq!(schema, fields, "profile schema/struct field drift");
    }

    #[test]
    fn similarity_defaults_track_the_constants() {
        // The effective defaults are documented as literals; if the constants
        // move, this fails so the schema rows get updated with them.
        let md = render();
        assert!(
            md.contains(&format!("default: {DEFAULT_SIMILARITY_THRESHOLD}")),
            "threshold default not documented as {DEFAULT_SIMILARITY_THRESHOLD}: {md}",
        );
        assert!(
            md.contains(&format!("default: {DEFAULT_SIMILARITY_MIN_LINES}")),
            "min-lines default not documented as {DEFAULT_SIMILARITY_MIN_LINES}: {md}",
        );
    }

    #[test]
    fn render_covers_profile_keys_and_every_tool_table() {
        let md = render();
        assert!(md.starts_with("# agent-lens.toml schema\n"), "got: {md}");

        // Shared profile keys.
        for key in [
            "path",
            "tools",
            "format",
            "exclude",
            "only-tests",
            "exclude-tests",
        ] {
            assert!(
                md.contains(&format!("`{key}`")),
                "missing profile key {key}: {md}"
            );
        }

        // Every analyzer gets a heading, and the option-bearing ones expose
        // a representative key.
        for tool in TOOL_ORDER {
            let heading = format!("### `[profile.<name>.{}]`", tool.as_str());
            assert!(md.contains(&heading), "missing heading for {tool:?}: {md}");
        }
        assert!(md.contains("`threshold`"), "got: {md}");
        assert!(md.contains("`since`"), "got: {md}");
        assert!(md.contains("`entry-glob`"), "got: {md}");

        // Option-less analyzers say so rather than showing an empty table.
        assert!(
            md.contains("No options. Declaring this table is a parse error."),
            "got: {md}",
        );
    }

    #[test]
    fn tool_table_marks_only_cycles_and_function_graph_as_optionless() {
        for tool in TOOL_ORDER {
            let has_table = tool_command(tool).is_some();
            let expect_table = !matches!(tool, ToolName::Cycles | ToolName::FunctionGraph);
            assert_eq!(has_table, expect_table, "mismatch for {tool:?}");
        }
    }

    #[test]
    fn every_table_row_has_four_columns() {
        // A literal `|` inside a cell silently breaks the column layout, so
        // every table line must carry exactly five separators.
        for line in render().lines().filter(|l| l.starts_with("| ")) {
            assert_eq!(
                line.matches('|').count() - line.matches("\\|").count(),
                5,
                "table row is not 4 columns: {line}",
            );
        }
    }

    #[test]
    fn example_is_a_valid_config() {
        // The worked example must parse, or the schema doc ships a config
        // an agent would copy and get a parse error from.
        let config: crate::config::Config = toml::from_str(EXAMPLE).unwrap();
        assert!(config.profile("web").is_ok());
        assert!(config.profile("backend").is_ok());
        assert_eq!(
            config.profile("services").unwrap().path.paths(),
            [
                std::path::PathBuf::from("internal"),
                std::path::PathBuf::from("cmd")
            ],
        );
    }

    /// A runtime default names a real key, and only one clap does not
    /// already default — otherwise the struct and this table disagree
    /// about which value wins.
    #[test]
    fn runtime_defaults_name_keys_without_a_clap_default() {
        for &(tool, key, _) in RUNTIME_DEFAULTS {
            let command = tool_command(tool).unwrap_or_else(|| panic!("{tool:?} has no options"));
            let arg = command
                .get_arguments()
                .find(|a| a.get_long() == Some(key))
                .unwrap_or_else(|| panic!("{tool:?} has no `{key}` option"));
            assert!(
                arg.get_default_values().is_empty(),
                "{tool:?}.{key} already has a clap default; drop the runtime entry",
            );
        }
    }

    /// The generated rows read the options struct, so a value enum lists
    /// every variant — the hand-written table once omitted `blocks`.
    #[rstest::rstest]
    #[case("| `target` | \"functions\", \"types\" or \"blocks\" | default: functions |")]
    #[case("| `weights` | \"static\", \"co-change\" or \"both\" | default: static |")]
    #[case("| `diff-only` | bool | default: false |")]
    #[case("| `limit` | int | default: 20 |")]
    // `graph-query` has its own `limit`: a runtime default belongs to one
    // tool, not to every key spelled the same.
    #[case("| `limit` | int | default: 50 |")]
    #[case("| `sweep` | array<float> | default: [] |")]
    #[case("| `bot-pattern` | array<string> | default: [\"*\\\\[bot\\\\]*\"] |")]
    #[case("| `query` | \"callers\", \"callees\", \"neighborhood\" or \"path\" | required |")]
    fn rows_are_generated_from_the_options_structs(#[case] row: &str) {
        let md = render();
        assert!(md.contains(row), "missing `{row}` in:\n{md}");
    }

    #[rstest::rstest]
    #[case(&[], "")]
    #[case(&["\"a\""], "\"a\"")]
    #[case(&["\"a\"", "\"b\""], "\"a\" or \"b\"")]
    #[case(&["\"a\"", "\"b\"", "\"c\""], "\"a\", \"b\" or \"c\"")]
    fn choices_read_as_an_english_list(#[case] items: &[&str], #[case] expected: &str) {
        let owned: Vec<String> = items.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(english_or(&owned), expected);
    }
}

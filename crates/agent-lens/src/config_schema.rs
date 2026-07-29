//! Render the `agent-lens.toml` schema as agent-friendly Markdown.
//!
//! `agent-lens config schema` exists because the config format lives only
//! in the serde structs of [`crate::config`] — there is no published JSON
//! Schema and the keys are not documented elsewhere. Rather than make an
//! agent read `config.rs`, this emits a dense, decoration-free reference:
//! the `[profile.<name>]` keys, every per-tool option table, and a worked
//! example, in the same spirit as [`crate::help_md`].
//!
//! The per-tool tables are produced from an exhaustive `match` on
//! [`ToolName`] (see [`tool_table`]), so adding an analyzer to the config
//! fails to compile until its schema entry is filled in. Field-level drift
//! within a table is not a compile error — the `Field` arrays are literals
//! decoupled from the structs — but the `schema_keys_match_struct_fields`
//! parity test reflects each struct's serde field list and fails loudly in
//! both directions (a struct field missing from the schema, or a schema key
//! whose struct field was removed).

use std::fmt::Write as _;

use crate::config::{CONFIG_FILE_NAME, ToolName};

/// Order the per-tool tables are rendered in. Kept in sync with the
/// exhaustive `match` in [`tool_table`]; a missing variant there is a
/// compile error, and the cohesion test guards the reverse direction.
const TOOL_ORDER: [ToolName; 16] = [
    ToolName::Similarity,
    ToolName::Complexity,
    ToolName::Cohesion,
    ToolName::Hotspot,
    ToolName::Hubs,
    ToolName::Impact,
    ToolName::Layers,
    ToolName::Untested,
    ToolName::Visibility,
    ToolName::Delegation,
    ToolName::GraphQuery,
    ToolName::ContextSpan,
    ToolName::Wrapper,
    ToolName::Coupling,
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
    /// What the key controls. Mirrors the doc comment in `config.rs`.
    desc: &'static str,
}

/// The `[profile.<name>.<tool>]` options table for one analyzer, or
/// `None` for analyzers that take no overrides.
struct ToolTable {
    fields: &'static [Field],
}

/// Top-level `[profile.<name>]` keys shared by every analyzer.
const PROFILE_FIELDS: &[Field] = &[
    Field {
        key: "path",
        ty: "string (path)",
        presence: "required",
        desc: "Target path handed to every analyzer in `tools`. A relative path is resolved against the directory holding agent-lens.toml.",
    },
    Field {
        key: "tools",
        ty: "array<tool-name>",
        presence: "required",
        desc: "Analyzers to run, in order. Each entry is one of: cohesion, complexity, coupling, context-span, cycles, delegation, function-graph, graph-query, hotspot, hubs, impact, layers, similarity, untested, visibility, wrapper.",
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

/// Per-tool override fields, keyed by analyzer.
///
/// This `match` is exhaustive on purpose: a new [`ToolName`] variant will
/// not compile until its schema entry is added here, so the rendered
/// reference stays in lockstep with the config structs.
fn tool_table(tool: ToolName) -> Option<ToolTable> {
    let fields: &'static [Field] = match tool {
        ToolName::Similarity => &[
            Field {
                key: "threshold",
                ty: "float",
                presence: "default: 0.85",
                desc: "Similarity cut for clustering. Omitting it applies the default, not \"no cut\".",
            },
            Field {
                key: "sweep",
                ty: "array<float>",
                presence: "optional",
                desc: "Multi-threshold sweep ladder. When set it supersedes threshold as the clustering cut.",
            },
            Field {
                key: "min-lines",
                ty: "int",
                presence: "default: 5",
                desc: "Ignore functions shorter than this many lines. Omitting it applies the default, not \"no floor\".",
            },
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the report to the top N results.",
            },
            Field {
                key: "method",
                ty: "\"tsed\" or \"token\"",
                presence: "optional",
                desc: "Body-scoring algorithm: tsed (tree-edit distance, default) or token (k-gram overlap).",
            },
            Field {
                key: "doc-overlap",
                ty: "bool",
                presence: "default: false",
                desc: "Roll the per-pair doc-comment overlap up into the markdown report. Diagnostic only; it never feeds the similarity score, and JSON output carries the per-pair values either way.",
            },
            Field {
                key: "diff-only",
                ty: "bool",
                presence: "default: false",
                desc: "Restrict analysis to functions touched by the working-tree diff.",
            },
        ],
        ToolName::Complexity => &[
            Field {
                key: "min-score",
                ty: "int",
                presence: "optional",
                desc: "Drop functions scoring below this threshold.",
            },
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the report to the top N results.",
            },
            Field {
                key: "diff-only",
                ty: "bool",
                presence: "default: false",
                desc: "Restrict analysis to functions touched by the working-tree diff.",
            },
        ],
        ToolName::Cohesion => &[
            Field {
                key: "min-score",
                ty: "int",
                presence: "optional",
                desc: "Drop units scoring below this threshold.",
            },
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the report to the top N results.",
            },
            Field {
                key: "diff-only",
                ty: "bool",
                presence: "default: false",
                desc: "Restrict analysis to units touched by the working-tree diff.",
            },
        ],
        ToolName::Hotspot => &[
            Field {
                key: "since",
                ty: "string",
                presence: "optional",
                desc: "Git revision window for churn, e.g. \"90.days.ago\".",
            },
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the report to the top N results.",
            },
        ],
        ToolName::Hubs => &[Field {
            key: "top",
            ty: "int",
            presence: "optional",
            desc: "Cap each markdown ranking to the top N results.",
        }],
        ToolName::Impact => &[
            Field {
                key: "function",
                ty: "array<string>",
                presence: "default: []",
                desc: "Seed symbols (qualified-name suffix or exact node id). When empty, seeds come from the unstaged working-tree diff.",
            },
            Field {
                key: "depth",
                ty: "int",
                presence: "default: 5",
                desc: "Reverse-traversal depth cap in call hops (cycles count as one).",
            },
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the markdown caller and test lists to the top N rows.",
            },
        ],
        ToolName::Layers => &[Field {
            key: "top",
            ty: "int",
            presence: "optional",
            desc: "Cap each markdown listing to the top N rows.",
        }],
        ToolName::Untested => &[Field {
            key: "top",
            ty: "int",
            presence: "optional",
            desc: "Cap the markdown module listing to the top N modules.",
        }],
        ToolName::Visibility => &[Field {
            key: "top",
            ty: "int",
            presence: "optional",
            desc: "Cap the markdown module listing to the top N modules.",
        }],
        ToolName::Delegation => &[
            Field {
                key: "top",
                ty: "int",
                presence: "optional",
                desc: "Cap the markdown chain and module listings to the top N rows.",
            },
            Field {
                key: "diff-only",
                ty: "bool",
                presence: "default: false",
                desc: "Keep only chains with a hop or terminus on an unstaged changed line.",
            },
        ],
        // The only tool whose options table is mandatory when the tool
        // is listed: a traversal needs a verb and a start symbol.
        ToolName::GraphQuery => &[
            Field {
                key: "query",
                ty: "\"callers\" / \"callees\" / \"neighborhood\" / \"path\"",
                presence: "required",
                desc: "Traversal verb to run.",
            },
            Field {
                key: "symbol",
                ty: "string",
                presence: "required",
                desc: "Function to start from: a `::`-segment suffix of its qualified name, or an exact node id.",
            },
            Field {
                key: "to",
                ty: "string",
                presence: "optional",
                desc: "Destination symbol. Required by the path query, invalid for the others.",
            },
            Field {
                key: "depth",
                ty: "int",
                presence: "default: 1 (path: unbounded)",
                desc: "Traversal depth cap in call hops.",
            },
            Field {
                key: "direction",
                ty: "\"in\" / \"out\" / \"both\"",
                presence: "default: both",
                desc: "Traversal direction. Only valid for the neighborhood query.",
            },
            Field {
                key: "limit",
                ty: "int",
                presence: "default: 50",
                desc: "Cap the result set by node count.",
            },
        ],
        ToolName::ContextSpan => &[Field {
            key: "entry-glob",
            ty: "array<string> (globs)",
            presence: "default: []",
            desc: "Entry-point globs used to seed the span walk.",
        }],
        ToolName::Wrapper => &[Field {
            key: "diff-only",
            ty: "bool",
            presence: "default: false",
            desc: "Restrict analysis to functions touched by the working-tree diff.",
        }],
        // Analyzers that take no per-tool overrides. Declaring a
        // `[profile.<name>.coupling]` table is a parse error.
        ToolName::Coupling | ToolName::Cycles | ToolName::FunctionGraph => return None,
    };
    Some(ToolTable { fields })
}

/// A compact, valid config that exercises a profile, shared filters, and
/// two per-tool override tables.
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
since = \"90.days.ago\"";

/// Render the whole `agent-lens.toml` schema as one Markdown document.
pub fn render() -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# {CONFIG_FILE_NAME} schema");
    out.push_str(
        "\nNamed analysis profiles for the `run` subcommand. The nearest \
         file is discovered by walking up from the current directory (or \
         pointed at with `--config`). Keys are kebab-case to mirror the CLI \
         flags, and unknown keys are a parse error.\n",
    );

    let _ = writeln!(out, "\n## `[profile.<name>]`");
    out.push_str(
        "\nOne table per profile. `<name>` is the argument passed to \
         `agent-lens run <name>`.\n",
    );
    render_fields(&mut out, PROFILE_FIELDS);

    out.push_str(
        "\n## Per-tool overrides\n\nEach `[profile.<name>.<tool>]` table is \
         optional; an absent table runs the analyzer with its CLI defaults. \
         A table for an analyzer that is not listed in `tools` is still \
         parsed but never applied.\n",
    );

    for tool in TOOL_ORDER {
        let name = tool.as_str();
        let _ = writeln!(out, "\n### `[profile.<name>.{name}]`");
        match tool_table(tool) {
            Some(table) => render_fields(&mut out, table.fields),
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

/// Render a slice of fields as a Markdown table.
fn render_fields(out: &mut String, fields: &[Field]) {
    out.push_str("\n| Key | Type | Presence | Description |\n| --- | --- | --- | --- |\n");
    for field in fields {
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {} |",
            field.key, field.ty, field.presence, field.desc,
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
        CohesionOptions, ComplexityOptions, ContextSpanOptions, DelegationOptions,
        GraphQueryOptions, HotspotOptions, HubsOptions, ImpactOptions, LayersOptions, Profile,
        SimilarityOptions, UntestedOptions, VisibilityOptions, WrapperOptions,
    };

    /// Schema keys documented for `tool` must match, exactly, the serde field
    /// names of its options struct `T`. Catches both drift directions: a new
    /// struct field missing from the schema, and a schema key whose field was
    /// removed.
    fn assert_tool_parity<'de, T: serde::Deserialize<'de>>(tool: ToolName) {
        let schema: BTreeSet<&str> = tool_table(tool)
            .unwrap_or_else(|| panic!("{tool:?} should have an options table"))
            .fields
            .iter()
            .map(|f| f.key)
            .collect();
        let fields: BTreeSet<&str> = reflect::field_names::<T>().into_iter().collect();
        assert_eq!(schema, fields, "schema/struct field drift for {tool:?}");
    }

    #[test]
    fn schema_keys_match_struct_fields() {
        assert_tool_parity::<SimilarityOptions>(ToolName::Similarity);
        assert_tool_parity::<ComplexityOptions>(ToolName::Complexity);
        assert_tool_parity::<CohesionOptions>(ToolName::Cohesion);
        assert_tool_parity::<HotspotOptions>(ToolName::Hotspot);
        assert_tool_parity::<HubsOptions>(ToolName::Hubs);
        assert_tool_parity::<ImpactOptions>(ToolName::Impact);
        assert_tool_parity::<LayersOptions>(ToolName::Layers);
        assert_tool_parity::<UntestedOptions>(ToolName::Untested);
        assert_tool_parity::<VisibilityOptions>(ToolName::Visibility);
        assert_tool_parity::<DelegationOptions>(ToolName::Delegation);
        assert_tool_parity::<GraphQueryOptions>(ToolName::GraphQuery);
        assert_tool_parity::<ContextSpanOptions>(ToolName::ContextSpan);
        assert_tool_parity::<WrapperOptions>(ToolName::Wrapper);
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
                    .filter(|&t| tool_table(t).is_some())
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
    fn tool_table_marks_only_coupling_cycles_and_function_graph_as_optionless() {
        for tool in TOOL_ORDER {
            let has_table = tool_table(tool).is_some();
            let expect_table = !matches!(
                tool,
                ToolName::Coupling | ToolName::Cycles | ToolName::FunctionGraph
            );
            assert_eq!(has_table, expect_table, "mismatch for {tool:?}");
        }
    }

    #[test]
    fn every_table_row_has_four_columns() {
        // A literal `|` inside a cell silently breaks the column layout, so
        // every table line must carry exactly five separators.
        for line in render().lines().filter(|l| l.starts_with("| ")) {
            assert_eq!(
                line.matches('|').count(),
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
    }
}

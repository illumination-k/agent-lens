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
//! fails to compile until its schema entry is filled in — the schema
//! cannot silently drift away from the structs it documents.

use std::fmt::Write as _;

use crate::config::{CONFIG_FILE_NAME, ToolName};

/// Order the per-tool tables are rendered in. Kept in sync with the
/// exhaustive `match` in [`tool_table`]; a missing variant there is a
/// compile error, and the cohesion test guards the reverse direction.
const TOOL_ORDER: [ToolName; 8] = [
    ToolName::Similarity,
    ToolName::Complexity,
    ToolName::Cohesion,
    ToolName::Hotspot,
    ToolName::ContextSpan,
    ToolName::Wrapper,
    ToolName::Coupling,
    ToolName::FunctionGraph,
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
        desc: "Analyzers to run, in order. Each entry is one of: cohesion, complexity, coupling, context-span, function-graph, hotspot, similarity, wrapper.",
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
                presence: "optional",
                desc: "Similarity cut for clustering.",
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
                presence: "optional",
                desc: "Ignore functions shorter than this many lines.",
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
        ToolName::Coupling | ToolName::FunctionGraph => return None,
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn tool_table_marks_only_coupling_and_function_graph_as_optionless() {
        for tool in TOOL_ORDER {
            let has_table = tool_table(tool).is_some();
            let expect_table = !matches!(tool, ToolName::Coupling | ToolName::FunctionGraph);
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

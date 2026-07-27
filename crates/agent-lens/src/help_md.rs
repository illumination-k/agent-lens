//! Render an agent-friendly Markdown reference of the whole CLI.
//!
//! Walks a [`clap::Command`] tree and emits a dense, decoration-free
//! Markdown document: one heading per (sub)command with its description
//! and an options table. The point is to give a coding agent the entire
//! command surface in one place — `agent-lens help --md` drops straight
//! into an LLM context instead of making the agent run `--help` on every
//! subcommand and stitch the pieces together.
//!
//! This is deliberately not pretty terminal output: no colours, no ASCII
//! framing, just the commands, descriptions, options, defaults, and
//! accepted values an agent needs to pick the right invocation.

use std::fmt::Write as _;

use clap::{Arg, ArgAction, Command};

/// Render `root` and every nested subcommand as one Markdown document.
pub fn render(root: &Command) -> String {
    let mut out = String::new();
    let name = root.get_name();
    let _ = writeln!(out, "# {name}");
    if let Some(version) = root.get_version() {
        let _ = writeln!(out, "\nVersion: {version}");
    }
    if let Some(about) = describe(root) {
        let _ = writeln!(out, "\n{about}");
    }
    render_options(&mut out, root);
    render_epilogue(&mut out, root);
    render_index(&mut out, root);
    for sub in visible_subcommands(root) {
        render_command(&mut out, sub, &[name]);
    }
    out
}

/// A flat index of every command with its one-line purpose, so an agent
/// can scan the whole surface before deciding which section to read. The
/// per-command sections below carry the full prose.
fn render_index(out: &mut String, root: &Command) {
    let mut rows = Vec::new();
    collect_index(&mut rows, root, &[root.get_name()]);
    if rows.is_empty() {
        return;
    }

    out.push_str("\n## Command index\n\n| Command | Purpose |\n| --- | --- |\n");
    for (path, summary) in rows {
        let _ = writeln!(out, "| `{path}` | {summary} |");
    }
}

/// Depth-first walk collecting `(command path, one-line summary)` in the
/// same order the document lists the sections.
fn collect_index(rows: &mut Vec<(String, String)>, cmd: &Command, parent_path: &[&str]) {
    for sub in visible_subcommands(cmd) {
        let mut path = parent_path.to_vec();
        path.push(sub.get_name());
        rows.push((path.join(" "), summarize(sub)));
        collect_index(rows, sub, &path);
    }
}

/// Emit one command's section, then recurse into its visible
/// subcommands. The heading depth tracks the path depth so the document
/// nests `##` / `###` / `####` the way the command tree does.
fn render_command(out: &mut String, cmd: &Command, parent_path: &[&str]) {
    let mut path = parent_path.to_vec();
    path.push(cmd.get_name());

    let hashes = "#".repeat(path.len().min(6));
    let _ = writeln!(out, "\n{hashes} `{}`", path.join(" "));
    if let Some(about) = describe(cmd) {
        let _ = writeln!(out, "\n{about}");
    }
    render_options(out, cmd);
    render_epilogue(out, cmd);

    for sub in visible_subcommands(cmd) {
        render_command(out, sub, &path);
    }
}

/// Append the command's `after_long_help` block — the worked examples.
/// Those blocks are authored as Markdown-compatible text (prose plus
/// four-space-indented command listings), so they pass through verbatim.
fn render_epilogue(out: &mut String, cmd: &Command) {
    let Some(epilogue) = cmd
        .get_after_long_help()
        .or_else(|| cmd.get_after_help())
        .map(|help| help.to_string())
        .map(|help| help.trim_end().to_string())
        .filter(|help| !help.is_empty())
    else {
        return;
    };
    let _ = writeln!(out, "\n{epilogue}");
}

/// Render the command's arguments as a Markdown table, skipping the
/// auto-generated `help` / `version` flags. Emits nothing when the only
/// arguments are those built-ins.
fn render_options(out: &mut String, cmd: &Command) {
    let args: Vec<&Arg> = cmd
        .get_arguments()
        .filter(|a| !a.is_hide_set())
        .filter(|a| !matches!(a.get_id().as_str(), "help" | "version"))
        .collect();
    if args.is_empty() {
        return;
    }

    out.push_str("\n| Argument | Description |\n| --- | --- |\n");
    for arg in args {
        let token = arg_token(arg);
        let desc = arg_description(arg);
        let _ = writeln!(out, "| `{token}` | {desc} |");
    }
}

/// The CLI spelling of an argument: `<PATH>` for a positional, or
/// `--long <VALUE>` (falling back to a short flag) for an option.
fn arg_token(arg: &Arg) -> String {
    if arg.is_positional() {
        return format!("<{}>", value_name(arg));
    }
    let mut token = match (arg.get_long(), arg.get_short()) {
        (Some(long), _) => format!("--{long}"),
        (None, Some(short)) => format!("-{short}"),
        (None, None) => arg.get_id().to_string(),
    };
    if arg.get_action().takes_values() {
        let _ = write!(token, " <{}>", value_name(arg));
    }
    token
}

/// The placeholder for an argument's value: its first declared value
/// name, or the upper-cased argument id when none was set.
fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

/// One table cell: the help text plus any accepted values / default,
/// flattened onto a single line so it stays inside a Markdown table row.
fn arg_description(arg: &Arg) -> String {
    let mut desc = arg
        .get_long_help()
        .or_else(|| arg.get_help())
        .map(|help| help.to_string())
        .unwrap_or_default();
    desc = desc.split_whitespace().collect::<Vec<_>>().join(" ");
    desc = desc.replace('|', "\\|");

    let extra = arg_annotations(arg);
    if extra.is_empty() {
        desc
    } else if desc.is_empty() {
        format!("({})", extra.join("; "))
    } else {
        format!("{desc} ({})", extra.join("; "))
    }
}

/// The parenthesised annotations trailing an argument's help text: the
/// facts an agent needs to build a valid invocation but which clap only
/// exposes structurally — whether the argument must be supplied, what it
/// accepts, what it defaults to, whether it can repeat, and what else it
/// answers to.
fn arg_annotations(arg: &Arg) -> Vec<String> {
    let mut extra: Vec<String> = Vec::new();
    if arg.is_required_set() {
        extra.push("required".to_string());
    }
    // Only value-taking options have meaningful accepted-values and
    // default annotations; for boolean flags the `true`/`false` pair and
    // the implicit `false` default are noise, so skip them.
    if arg.get_action().takes_values() {
        extra.extend(labelled(
            "values",
            arg.get_possible_values().iter(),
            |value| value.get_name().to_string(),
        ));
        extra.extend(labelled(
            "default",
            arg.get_default_values().iter(),
            |value| value.to_string_lossy().into_owned(),
        ));
    }
    if matches!(arg.get_action(), ArgAction::Append) {
        extra.push("repeatable".to_string());
    }
    extra.extend(labelled(
        "alias",
        arg.get_visible_aliases().unwrap_or_default().iter(),
        |alias| format!("--{alias}"),
    ));
    extra
}

/// `Some("label: a, b")` for a non-empty list of rendered items, nothing
/// at all for an empty one — an empty annotation is noise, not a fact.
fn labelled<T>(
    label: &str,
    items: impl Iterator<Item = T>,
    render: impl Fn(T) -> String,
) -> Option<String> {
    let rendered: Vec<String> = items.map(render).collect();
    (!rendered.is_empty()).then(|| format!("{label}: {}", rendered.join(", ")))
}

/// The command's one-line purpose for the index: its short `about`, or
/// the first paragraph of the long one folded onto a single line. Pipes
/// are escaped so the summary survives the Markdown table.
fn summarize(cmd: &Command) -> String {
    let text = cmd
        .get_about()
        .map(|about| about.to_string())
        .or_else(|| {
            cmd.get_long_about()
                .map(|about| about.to_string())
                .map(|about| {
                    about
                        .split("\n\n")
                        .next()
                        .unwrap_or_default()
                        .trim()
                        .to_string()
                })
        })
        .unwrap_or_default();
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace('|', "\\|")
}

/// Prefer a command's long description over its short one, trimmed and
/// dropped entirely when empty.
fn describe(cmd: &Command) -> Option<String> {
    cmd.get_long_about()
        .or_else(|| cmd.get_about())
        .map(|about| about.to_string().trim().to_string())
        .filter(|about| !about.is_empty())
}

/// Subcommands worth documenting: visible ones, minus the auto-generated
/// `help` subcommand clap injects into every command that has children.
fn visible_subcommands(cmd: &Command) -> impl Iterator<Item = &Command> {
    cmd.get_subcommands()
        .filter(|sub| !sub.is_hide_set() && sub.get_name() != "help")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction, Command};
    use rstest::rstest;

    fn sample() -> Command {
        Command::new("demo")
            .version("1.2.3")
            .about("Demo about line.")
            .after_long_help("Examples:\n\n    demo analyze .\n")
            .subcommand(
                Command::new("analyze")
                    .about("Analyze about line.")
                    .long_about("Run an analyzer.\nSecond paragraph.")
                    .after_long_help("Examples:\n\n    demo analyze src/ --format md\n")
                    .arg(Arg::new("path").required(true).help("Path to analyze."))
                    .arg(
                        Arg::new("format")
                            .long("format")
                            .help("Output format.")
                            .value_parser(["json", "md"])
                            .default_value("json"),
                    )
                    .arg(
                        Arg::new("diff_only")
                            .long("diff-only")
                            .action(ArgAction::SetTrue)
                            .help("Restrict to the diff."),
                    )
                    .arg(
                        Arg::new("exclude")
                            .long("exclude")
                            .value_name("GLOB")
                            .action(ArgAction::Append)
                            .help("Exclude a glob."),
                    )
                    .arg(
                        Arg::new("threshold")
                            .long("threshold")
                            .visible_alias("min-score")
                            .help("Similarity threshold."),
                    )
                    .subcommand(Command::new("nested").about("A nested command.")),
            )
    }

    #[test]
    fn render_includes_title_version_and_about() {
        let md = render(&sample());
        assert!(md.starts_with("# demo\n"), "got: {md}");
        assert!(md.contains("Version: 1.2.3"), "got: {md}");
        assert!(md.contains("Demo about line."), "got: {md}");
    }

    #[test]
    fn render_emits_command_path_headings() {
        let md = render(&sample());
        assert!(md.contains("## `demo analyze`"), "got: {md}");
        // The nested command sits one level deeper in the tree.
        assert!(md.contains("### `demo analyze nested`"), "got: {md}");
    }

    #[test]
    fn render_prefers_long_about_over_about() {
        let md = render(&sample());
        assert!(md.contains("Second paragraph."), "got: {md}");
    }

    #[rstest]
    #[case::positional("| `<PATH>` | Path to analyze. (required) |")]
    #[case::valued("| `--format <FORMAT>` | Output format. (values: json, md; default: json) |")]
    #[case::flag("| `--diff-only` | Restrict to the diff. |")]
    #[case::appended("| `--exclude <GLOB>` | Exclude a glob. (repeatable) |")]
    #[case::aliased("| `--threshold <THRESHOLD>` | Similarity threshold. (alias: --min-score) |")]
    fn render_documents_positionals_options_and_flags(#[case] row: &str) {
        let md = render(&sample());
        assert!(md.contains(row), "missing {row}\ngot: {md}");
    }

    #[test]
    fn render_skips_builtin_help_and_version_flags() {
        let md = render(&sample());
        assert!(!md.contains("--help"), "got: {md}");
        assert!(!md.contains("Print help"), "got: {md}");
    }

    #[rstest]
    #[case("    demo analyze .")]
    #[case("    demo analyze src/ --format md")]
    fn render_includes_after_long_help_examples(#[case] line: &str) {
        let md = render(&sample());
        assert!(md.contains(line), "missing {line}\ngot: {md}");
    }

    #[rstest]
    #[case("## Command index")]
    #[case("| `demo analyze` | Analyze about line. |")]
    #[case("| `demo analyze nested` | A nested command. |")]
    fn render_opens_with_a_command_index(#[case] row: &str) {
        let md = render(&sample());
        let index = md.find("## Command index").expect("index heading");
        let first_section = md.find("## `demo analyze`").expect("first section");
        assert!(index < first_section, "index must precede sections: {md}");
        assert!(md.contains(row), "missing {row}\ngot: {md}");
    }

    #[test]
    fn index_summary_falls_back_to_the_first_long_about_paragraph() {
        let cmd = Command::new("x").long_about("First line.\n\nSecond paragraph.");
        assert_eq!(summarize(&cmd), "First line.");
    }

    #[test]
    fn index_summary_escapes_pipes() {
        let cmd = Command::new("x").about("a | b");
        assert_eq!(summarize(&cmd), "a \\| b");
    }

    #[test]
    fn arg_description_escapes_pipes() {
        let arg = Arg::new("x").long("x").help("a | b");
        assert_eq!(arg_description(&arg), "a \\| b");
    }
}

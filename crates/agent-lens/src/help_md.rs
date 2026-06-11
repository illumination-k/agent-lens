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

use clap::{Arg, Command};

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
    for sub in visible_subcommands(root) {
        render_command(&mut out, sub, &[name]);
    }
    out
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

    for sub in visible_subcommands(cmd) {
        render_command(out, sub, &path);
    }
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

    // Only value-taking options have meaningful accepted-values and
    // default annotations; for boolean flags the `true`/`false` pair and
    // the implicit `false` default are noise, so skip them.
    let mut extra: Vec<String> = Vec::new();
    if arg.get_action().takes_values() {
        let values: Vec<String> = arg
            .get_possible_values()
            .iter()
            .map(|value| value.get_name().to_string())
            .collect();
        if !values.is_empty() {
            extra.push(format!("values: {}", values.join(", ")));
        }
        let defaults: Vec<String> = arg
            .get_default_values()
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        if !defaults.is_empty() {
            extra.push(format!("default: {}", defaults.join(", ")));
        }
    }

    if extra.is_empty() {
        desc
    } else if desc.is_empty() {
        format!("({})", extra.join("; "))
    } else {
        format!("{desc} ({})", extra.join("; "))
    }
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

    fn sample() -> Command {
        Command::new("demo")
            .version("1.2.3")
            .about("Demo about line.")
            .subcommand(
                Command::new("analyze")
                    .long_about("Run an analyzer.\nSecond paragraph.")
                    .arg(Arg::new("path").help("Path to analyze."))
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

    #[test]
    fn render_documents_positionals_options_and_flags() {
        let md = render(&sample());
        assert!(md.contains("| `<PATH>` | Path to analyze. |"), "got: {md}");
        assert!(
            md.contains(
                "| `--format <FORMAT>` | Output format. (values: json, md; default: json) |"
            ),
            "got: {md}",
        );
        assert!(
            md.contains("| `--diff-only` | Restrict to the diff. |"),
            "got: {md}",
        );
    }

    #[test]
    fn render_skips_builtin_help_and_version_flags() {
        let md = render(&sample());
        assert!(!md.contains("--help"), "got: {md}");
        assert!(!md.contains("Print help"), "got: {md}");
    }

    #[test]
    fn arg_description_escapes_pipes() {
        let arg = Arg::new("x").long("x").help("a | b");
        assert_eq!(arg_description(&arg), "a \\| b");
    }
}

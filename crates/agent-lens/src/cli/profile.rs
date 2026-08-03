//! The `agent-lens run` command: driving every analyzer in a named
//! `agent-lens.toml` profile and emitting one combined report.
//!
//! [`ResolvedProfile`] — config discovery, profile lookup, target
//! resolution, and the tool list — lives here too, because `baseline
//! create` drives the same profile through the same analyzers and only
//! differs in what it does with the reports.

use std::path::{Path, PathBuf};

use agent_lens::analyze::OutputFormat;
use agent_lens::config::{self, ConfigError};
use tracing::{info, warn};

use super::analyze::build_analyze_command;
use super::args::{ProfileSelectorArgs, RunArgs};
use super::write_stdout_line;

/// A named profile, ready to run: the profile itself plus the target
/// path already resolved against the config's directory.
///
/// The profile is cloned out of the [`config::Config`] rather than
/// borrowed from it so callers hold one self-contained value; a profile
/// is a handful of `Option`s and small `Vec`s, and this happens once per
/// command.
pub(super) struct ResolvedProfile {
    pub(super) name: String,
    pub(super) profile: config::Profile,
    pub(super) target: PathBuf,
}

impl ResolvedProfile {
    /// Discover (or take) the config, look the profile up, and resolve
    /// its target path.
    ///
    /// The target's existence is checked once here rather than per
    /// analyzer: every tool in the profile would otherwise fail the same
    /// way, and only this layer knows the path came from a config and
    /// what it was resolved against.
    pub(super) fn resolve(
        selector: ProfileSelectorArgs,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let config_path = match selector.config {
            Some(path) => path,
            None => {
                let cwd = std::env::current_dir()?;
                config::discover(&cwd).ok_or(ConfigError::NotFound { start: cwd })?
            }
        };
        let config = config::load(&config_path)?;
        let profile = config.profile(&selector.profile)?.clone();
        let config_dir = config_path.parent().unwrap_or_else(|| Path::new("."));
        let target = profile.resolved_path(config_dir);
        if !target.exists() {
            return Err(ConfigError::ProfilePathNotFound {
                name: selector.profile,
                path: profile.path,
                resolved: target,
            }
            .into());
        }

        for tool in unused_tool_option_tables(&profile) {
            warn!(
                profile = %selector.profile,
                tool = tool.as_str(),
                "options table set for a tool not listed in `tools`; ignored",
            );
        }

        Ok(Self {
            name: selector.profile,
            profile,
            target,
        })
    }

    /// The profile's tools in listed order, with repeats dropped —
    /// running an analyzer twice would only produce the same report
    /// twice, at twice the cost.
    pub(super) fn tools(&self) -> Vec<config::ToolName> {
        let mut seen = std::collections::HashSet::new();
        let mut tools = Vec::with_capacity(self.profile.tools.len());
        for &tool in &self.profile.tools {
            if seen.insert(tool) {
                tools.push(tool);
            } else {
                info!(
                    profile = %self.name,
                    tool = tool.as_str(),
                    "tool listed more than once in profile; running it only once",
                );
            }
        }
        tools
    }

    /// Run one of the profile's tools and hand back its rendered report.
    pub(super) fn run_tool(
        &self,
        tool: config::ToolName,
        format: OutputFormat,
    ) -> Result<String, Box<dyn std::error::Error>> {
        build_analyze_command(tool, &self.profile, &self.target, format)?.run()
    }
}

/// Run every analyzer in a named `agent-lens.toml` profile and emit one
/// combined report.
///
/// Each analyzer is driven through the same [`AnalyzeCommand`] the
/// `analyze` subcommand builds, so a profile run and the equivalent
/// hand-typed commands produce identical per-tool output.
pub(super) fn run_profile(args: RunArgs) -> Result<(), Box<dyn std::error::Error>> {
    let resolved = ResolvedProfile::resolve(args.selector)?;
    // `--format` beats the profile's `format`, which in turn beats the
    // JSON default: the flag is the caller speaking about this one run.
    let format = args
        .format
        .or(resolved.profile.format)
        .unwrap_or(OutputFormat::Json);

    let mut sections: Vec<(config::ToolName, String)> = Vec::new();
    for tool in resolved.tools() {
        sections.push((tool, resolved.run_tool(tool, format)?));
    }

    write_stdout_line(&render_profile_report(&resolved.name, format, &sections)?)
}

/// Tool-option tables (`[profile.<name>.<tool>]`) set for a tool the
/// profile's `tools` list never runs — their options would otherwise be
/// silently ignored, so `run` warns about each one.
fn unused_tool_option_tables(profile: &config::Profile) -> Vec<config::ToolName> {
    [
        (profile.similarity.is_some(), config::ToolName::Similarity),
        (profile.complexity.is_some(), config::ToolName::Complexity),
        (profile.cohesion.is_some(), config::ToolName::Cohesion),
        (profile.hotspot.is_some(), config::ToolName::Hotspot),
        (profile.risk.is_some(), config::ToolName::Risk),
        (profile.hubs.is_some(), config::ToolName::Hubs),
        (profile.impact.is_some(), config::ToolName::Impact),
        (profile.layers.is_some(), config::ToolName::Layers),
        (profile.graph_query.is_some(), config::ToolName::GraphQuery),
        (
            profile.context_span.is_some(),
            config::ToolName::ContextSpan,
        ),
        (profile.delegation.is_some(), config::ToolName::Delegation),
        (profile.unreachable.is_some(), config::ToolName::Unreachable),
        (profile.untested.is_some(), config::ToolName::Untested),
        (profile.visibility.is_some(), config::ToolName::Visibility),
        (profile.wrapper.is_some(), config::ToolName::Wrapper),
    ]
    .into_iter()
    .filter(|&(present, tool)| present && !profile.tools.contains(&tool))
    .map(|(_, tool)| tool)
    .collect()
}

/// Render the per-tool reports as one document: stacked `## <tool>`
/// sections for markdown, or a `{profile, results}` object for JSON where
/// each analyzer's JSON output is nested under its tool name.
fn render_profile_report(
    profile: &str,
    format: OutputFormat,
    sections: &[(config::ToolName, String)],
) -> Result<String, Box<dyn std::error::Error>> {
    Ok(match format {
        OutputFormat::Md => {
            let mut out = String::new();
            for (tool, report) in sections {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str("## ");
                out.push_str(tool.as_str());
                out.push_str("\n\n");
                out.push_str(report.trim_end_matches('\n'));
                out.push('\n');
            }
            out
        }
        OutputFormat::Json => {
            let mut results = Vec::with_capacity(sections.len());
            for (tool, report) in sections {
                let report: serde_json::Value = serde_json::from_str(report)?;
                results.push(serde_json::json!({ "tool": tool.as_str(), "report": report }));
            }
            serde_json::to_string(&serde_json::json!({
                "profile": profile,
                "results": results,
            }))?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_profile_report_md_stacks_tool_sections() {
        let sections = vec![
            (config::ToolName::Complexity, "complexity body\n".to_owned()),
            (config::ToolName::Wrapper, "wrapper body".to_owned()),
        ];
        let out = render_profile_report("audit", OutputFormat::Md, &sections).unwrap();
        // No leading newline, and a single blank line between sections.
        assert_eq!(
            out,
            "## complexity\n\ncomplexity body\n\n## wrapper\n\nwrapper body\n",
        );
    }

    #[test]
    fn tools_keeps_the_listed_order_and_runs_a_repeat_once() {
        let profile: config::Profile =
            toml::from_str("path = \"src\"\ntools = [\"wrapper\", \"complexity\", \"wrapper\"]\n")
                .unwrap();
        let resolved = ResolvedProfile {
            name: "audit".to_owned(),
            profile,
            target: PathBuf::from("src"),
        };
        assert_eq!(
            resolved.tools(),
            [config::ToolName::Wrapper, config::ToolName::Complexity],
        );
    }

    #[test]
    fn unused_tool_option_tables_flags_tables_off_the_tools_list() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.9\n\n[complexity]\nmin-score = 3\n\n[wrapper]\ndiff-only = true\n",
        )
        .unwrap();
        // similarity is listed in `tools`, so only complexity and wrapper
        // are flagged — in the fixed iteration order.
        assert_eq!(
            unused_tool_option_tables(&profile),
            [config::ToolName::Complexity, config::ToolName::Wrapper],
        );
    }

    #[test]
    fn unused_tool_option_tables_empty_when_every_table_is_listed() {
        let profile: config::Profile = toml::from_str(
            "path = \"web\"\ntools = [\"similarity\"]\n\n[similarity]\nthreshold = 0.9\n",
        )
        .unwrap();
        assert!(unused_tool_option_tables(&profile).is_empty());
    }

    #[test]
    fn render_profile_report_json_nests_each_tool_report() {
        let sections = vec![(config::ToolName::Complexity, "{\"k\":1}".to_owned())];
        let out = render_profile_report("audit", OutputFormat::Json, &sections).unwrap();
        let value: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(value["profile"], "audit");
        assert_eq!(value["results"][0]["tool"], "complexity");
        assert_eq!(value["results"][0]["report"]["k"], 1);
    }
}

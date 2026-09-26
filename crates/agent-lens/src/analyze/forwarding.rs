//! `analyze forwarding` — functions that only pass the call along.
//!
//! Two sections, each a full analyzer report in its own right:
//!
//! * `wrapper` — one hop: a function whose body, after stripping a short
//!   chain of trivial adapters, is a single forwarding call, with
//!   argument-level evidence;
//! * `delegation` — what those hops stack into: chains of forwarding-only
//!   functions ending at the terminus that does the work, and the modules
//!   built out of them.
//!
//! The sections share the per-file wrapper facts through the analysis
//! index. `--section` narrows the report; the JSON nests each section's
//! report under its key (`wrapper`, `delegation`) and lists the keys it
//! carries in `sections`.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use super::composite::{BundleSection, run_bundle};
use super::delegation::{DelegationAnalyzer, DelegationOptions};
use super::options::analyzer_options;
use super::wrapper::{WrapperAnalyzer, WrapperOptions};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

const NOTE: &str = "Functions that add nothing of their own before handing the call on. \
     `wrapper` lists each forwarding hop with the evidence that it only forwards; `delegation` \
     follows those hops into chains and names the terminus doing the work — the file to open \
     first. Both under-report on purpose: a forwarder that also logs, locks, or validates is \
     not one.";

/// One view of forwarding. Declaration order is the report order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ForwardingSection {
    /// Single forwarding hops, with argument-level evidence.
    Wrapper,
    /// Chains of forwarding hops and the modules built out of them.
    Delegation,
}

impl BundleSection for ForwardingSection {
    const ALL: &'static [Self] = &[Self::Wrapper, Self::Delegation];

    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Wrapper => ("wrapper", "wrapper"),
            Self::Delegation => ("delegation", "delegation"),
        }
    }
}

analyzer_options! {
    /// `analyze forwarding` flags, and the `[profile.<name>.forwarding]`
    /// table.
    pub struct ForwardingOptions {
        @shared(ranking, diff);
        /// Sections to report, comma-separated or repeated: `wrapper`,
        /// `delegation`. Defaults to both; they are always reported in
        /// that order.
        #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
        pub section: Vec<ForwardingSection>,
    }
}

/// Analyzer entry point for `analyze forwarding`.
#[derive(Debug, Default, Clone)]
pub struct ForwardingAnalyzer {
    wrapper: WrapperAnalyzer,
    delegation: DelegationAnalyzer,
    sections: Vec<ForwardingSection>,
}

/// Apply the same builder call to every section analyzer.
macro_rules! each_section {
    ($self:ident.$method:ident($arg:expr)) => {
        Self {
            wrapper: $self.wrapper.$method($arg.clone()),
            delegation: $self.delegation.$method($arg),
            sections: $self.sections,
        }
    };
}

impl ForwardingAnalyzer {
    /// Apply a whole [`ForwardingOptions`] group. The CLI flags and the
    /// `[profile.<name>.forwarding]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: ForwardingOptions) -> Self {
        let ForwardingOptions {
            top,
            diff_only,
            diff_range,
            section,
        } = opts;
        Self {
            wrapper: self.wrapper.with_options(WrapperOptions {
                top,
                diff_only,
                diff_range: diff_range.clone(),
            }),
            delegation: self.delegation.with_options(DelegationOptions {
                top,
                diff_only,
                diff_range,
            }),
            sections: section,
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_only_tests(self, only_tests: bool) -> Self {
        each_section!(self.with_only_tests(only_tests))
    }

    pub fn with_exclude_tests(self, exclude_tests: bool) -> Self {
        each_section!(self.with_exclude_tests(exclude_tests))
    }

    pub fn with_exclude_patterns(self, exclude: Vec<String>) -> Self {
        each_section!(self.with_exclude_patterns(exclude))
    }

    /// Restrict the report to these sections; empty means all of them.
    pub fn with_sections(mut self, sections: Vec<ForwardingSection>) -> Self {
        self.sections = sections;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        let roots = roots.into();
        run_bundle(
            "Forwarding",
            NOTE,
            &self.sections,
            format,
            |section| match section {
                ForwardingSection::Wrapper => self.wrapper.analyze(&roots, format),
                ForwardingSection::Delegation => self.delegation.analyze(&roots, format),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::Value;

    use super::*;
    use crate::test_support::write_file;

    /// `api::save` forwards to `service::save`, which forwards to
    /// `repo::save`, which forwards to `db::insert`, which does the work:
    /// three wrappers stacked into one chain.
    const FIXTURE: &str = r#"
pub mod api {
    pub fn save(id: usize) -> usize { crate::service::save(id) }
}
pub mod service {
    pub fn save(id: usize) -> usize { crate::repo::save(id) }
}
pub mod repo {
    pub fn save(id: usize) -> usize { crate::db::insert(id) }
}
pub mod db {
    pub fn insert(id: usize) -> usize {
        let mut total = 0;
        for i in 0..id { if i % 2 == 0 { total += i; } }
        total
    }
}
"#;

    fn analyze(analyzer: &ForwardingAnalyzer, format: OutputFormat) -> String {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        analyzer.analyze(file.as_path(), format).unwrap()
    }

    fn analyze_json(analyzer: &ForwardingAnalyzer) -> Value {
        serde_json::from_str(&analyze(analyzer, OutputFormat::Json)).unwrap()
    }

    #[test]
    fn sections_match_the_standalone_analyzers() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        let path = file.as_path();
        let json: Value = serde_json::from_str(
            &ForwardingAnalyzer::new()
                .analyze(path, OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            json["sections"],
            serde_json::json!(["wrapper", "delegation"])
        );
        let standalone = |body: Result<String, AnalyzerError>| {
            serde_json::from_str::<Value>(&body.unwrap()).unwrap()
        };
        assert_eq!(
            json["wrapper"],
            standalone(WrapperAnalyzer::new().analyze(path, OutputFormat::Json))
        );
        assert_eq!(
            json["delegation"],
            standalone(DelegationAnalyzer::new().analyze(path, OutputFormat::Json))
        );
    }

    #[test]
    fn each_section_finds_its_own_row() {
        let json = analyze_json(&ForwardingAnalyzer::new());
        assert_eq!(json["wrapper"]["wrapper_count"], 3, "{json}");
        let chains = json["delegation"]["chains"].to_string();
        assert!(chains.contains("insert"), "{chains}");
    }

    #[rstest]
    #[case(vec![ForwardingSection::Delegation], &["delegation"])]
    #[case(
        vec![ForwardingSection::Delegation, ForwardingSection::Wrapper],
        &["wrapper", "delegation"],
    )]
    fn section_selection_is_ordered(
        #[case] sections: Vec<ForwardingSection>,
        #[case] expected: &[&str],
    ) {
        let json = analyze_json(&ForwardingAnalyzer::new().with_sections(sections));
        assert_eq!(json["sections"], serde_json::json!(expected));
        for section in ForwardingSection::ALL {
            let (key, _) = section.labels();
            assert_eq!(json.get(key).is_some(), expected.contains(&key), "{key}");
        }
    }

    /// `--top` and the section choice land on the bundle's sections.
    #[test]
    fn options_reach_their_sections() {
        let opts = ForwardingOptions {
            top: Some(1),
            section: vec![ForwardingSection::Wrapper],
            ..ForwardingOptions::default()
        };
        let analyzer = ForwardingAnalyzer::new().with_options(opts);
        let md = analyze(&analyzer, OutputFormat::Md);
        assert!(md.starts_with("# Forwarding (wrapper)\n"), "{md}");
        let json = analyze_json(&analyzer);
        assert_eq!(json["sections"], serde_json::json!(["wrapper"]));
    }

    #[test]
    fn markdown_nests_every_section_under_one_title() {
        let md = analyze(&ForwardingAnalyzer::new(), OutputFormat::Md);
        assert!(
            md.starts_with("# Forwarding (wrapper, delegation)\n"),
            "{md}"
        );
        assert_eq!(
            md.lines().filter(|l| l.starts_with("# ")).count(),
            1,
            "{md}"
        );
        for heading in ["## Wrapper report", "## Delegation chains"] {
            assert!(md.contains(heading), "missing {heading}:\n{md}");
        }
    }
}

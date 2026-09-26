//! `analyze reach` — what the static call graph says about who reaches a
//! production function.
//!
//! Every production function sits in one cell of a two-by-two: reached
//! from a production entry point or not, reached from a test or not. The
//! three off-diagonal cells are the three sections of this report, each a
//! full analyzer report in its own right:
//!
//! | | reached from tests | not reached from tests |
//! | --- | --- | --- |
//! | reached from entries | fine, not listed | `untested` |
//! | not reached from entries | `test-only` | `unreachable` |
//!
//! The sections share one call graph and one raw-reference scan through
//! the analysis index, so asking all three questions costs little more
//! than asking one. `--section` narrows the report to the cells asked
//! for; the JSON nests each section's report under its key (`untested`,
//! `test_only`, `unreachable`) and lists the keys it carries in
//! `sections`.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use super::composite::{BundleSection, run_bundle};
use super::options::analyzer_options;
use super::test_only::{TestOnlyAnalyzer, TestOnlyOptions};
use super::unreachable::{Tier, UnreachableAnalyzer, UnreachableOptions};
use super::untested::{UntestedAnalyzer, UntestedOptions};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

const NOTE: &str = "Each production function sits in one cell of reached-from-entries × \
     reached-from-tests. `untested`: entries reach it, no test does. `test-only`: a test reaches \
     it, no production entry does. `unreachable`: neither does. Every section walks the same \
     static call graph over resolved edges only, so each carries its own bounds and caveats.";

/// One cell of the reach matrix. Declaration order is the report order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum ReachSection {
    /// Reached from a production entry point, never from a test.
    Untested,
    /// Reached from a test, never from a production entry point.
    TestOnly,
    /// Reached from neither.
    Unreachable,
}

impl BundleSection for ReachSection {
    const ALL: &'static [Self] = &[Self::Untested, Self::TestOnly, Self::Unreachable];

    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::Untested => ("untested", "untested"),
            Self::TestOnly => ("test_only", "test-only"),
            Self::Unreachable => ("unreachable", "unreachable"),
        }
    }
}

analyzer_options! {
    /// `analyze reach` flags, and the `[profile.<name>.reach]` table.
    pub struct ReachOptions {
        @shared(ranking);
        /// Sections to report, comma-separated or repeated: `untested`,
        /// `test-only`, `unreachable`. Defaults to all three; they are
        /// always reported in that order.
        #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
        pub section: Vec<ReachSection>,
        /// Lowest confidence tier the `unreachable` section renders in
        /// markdown: `confirmed` (default) leads with the deletable rows,
        /// `likely` adds the unused public surface, `unknown` adds every
        /// lead. JSON output always carries every tier.
        #[arg(long, value_enum)]
        pub tier: Option<Tier>,
    }
}

/// Analyzer entry point for `analyze reach`.
#[derive(Debug, Default, Clone)]
pub struct ReachAnalyzer {
    untested: UntestedAnalyzer,
    test_only: TestOnlyAnalyzer,
    unreachable: UnreachableAnalyzer,
    sections: Vec<ReachSection>,
}

/// Apply the same builder call to every section analyzer.
macro_rules! each_section {
    ($self:ident.$method:ident($arg:expr)) => {
        Self {
            untested: $self.untested.$method($arg.clone()),
            test_only: $self.test_only.$method($arg.clone()),
            unreachable: $self.unreachable.$method($arg),
            sections: $self.sections,
        }
    };
}

impl ReachAnalyzer {
    /// Apply a whole [`ReachOptions`] group. The CLI flags and the
    /// `[profile.<name>.reach]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: ReachOptions) -> Self {
        let ReachOptions { top, section, tier } = opts;
        Self {
            untested: self.untested.with_options(UntestedOptions { top }),
            test_only: self.test_only.with_options(TestOnlyOptions { top }),
            unreachable: self
                .unreachable
                .with_options(UnreachableOptions { top, tier }),
            sections: section,
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Keep only test-like files. Tests are what every section measures
    /// against, so each section says what that leaves it to judge.
    pub fn with_only_tests(self, only_tests: bool) -> Self {
        each_section!(self.with_only_tests(only_tests))
    }

    /// Drop test-like files — and with them the test half of the matrix,
    /// which each section reports as a cut entry set.
    pub fn with_exclude_tests(self, exclude_tests: bool) -> Self {
        each_section!(self.with_exclude_tests(exclude_tests))
    }

    pub fn with_exclude_patterns(self, exclude: Vec<String>) -> Self {
        each_section!(self.with_exclude_patterns(exclude))
    }

    /// Restrict the report to these sections; empty means all of them.
    pub fn with_sections(mut self, sections: Vec<ReachSection>) -> Self {
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
            "Reach",
            NOTE,
            &self.sections,
            format,
            |section| match section {
                ReachSection::Untested => self.untested.analyze(&roots, format),
                ReachSection::TestOnly => self.test_only.analyze(&roots, format),
                ReachSection::Unreachable => self.unreachable.analyze(&roots, format),
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

    /// `live` is reached from `main` and a test; `prod_only` from `main`
    /// alone; `helper` from a test alone; `dead` from nothing.
    const FIXTURE: &str = r#"
pub fn main() {
    live();
    prod_only();
}

fn live() {}

fn prod_only() {}

fn helper() {}

fn dead() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exercises() {
        live();
        helper();
    }
}
"#;

    fn analyze_json(analyzer: ReachAnalyzer) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        let out = analyzer
            .analyze(file.as_path(), OutputFormat::Json)
            .unwrap();
        serde_json::from_str(&out).unwrap()
    }

    fn names_under(value: &Value) -> String {
        value.to_string()
    }

    #[test]
    fn each_cell_of_the_matrix_lands_in_its_own_section() {
        let json = analyze_json(ReachAnalyzer::new());
        assert_eq!(
            json["sections"],
            serde_json::json!(["untested", "test_only", "unreachable"])
        );
        let untested = names_under(&json["untested"]);
        assert!(untested.contains("prod_only"), "{untested}");
        assert!(!untested.contains("\"lib::live\""), "{untested}");
        let test_only = names_under(&json["test_only"]);
        assert!(test_only.contains("helper"), "{test_only}");
        assert!(!test_only.contains("prod_only"), "{test_only}");
        let unreachable = names_under(&json["unreachable"]);
        assert!(unreachable.contains("dead"), "{unreachable}");
        assert!(!unreachable.contains("prod_only"), "{unreachable}");
    }

    #[test]
    fn sections_match_the_standalone_analyzers_byte_for_byte() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        let out = ReachAnalyzer::new()
            .analyze(file.as_path(), OutputFormat::Json)
            .unwrap();
        let json: Value = serde_json::from_str(&out).unwrap();
        let standalone = |body: String| serde_json::from_str::<Value>(&body).unwrap();
        assert_eq!(
            json["untested"],
            standalone(
                UntestedAnalyzer::new()
                    .analyze(file.as_path(), OutputFormat::Json)
                    .unwrap()
            )
        );
        assert_eq!(
            json["test_only"],
            standalone(
                TestOnlyAnalyzer::new()
                    .analyze(file.as_path(), OutputFormat::Json)
                    .unwrap()
            )
        );
        assert_eq!(
            json["unreachable"],
            standalone(
                UnreachableAnalyzer::new()
                    .analyze(file.as_path(), OutputFormat::Json)
                    .unwrap()
            )
        );
    }

    #[rstest]
    #[case(vec![ReachSection::Unreachable], &["unreachable"])]
    #[case(
        vec![ReachSection::Unreachable, ReachSection::Untested, ReachSection::Unreachable],
        &["untested", "unreachable"],
    )]
    fn section_selection_is_ordered_and_deduplicated(
        #[case] sections: Vec<ReachSection>,
        #[case] expected: &[&str],
    ) {
        let json = analyze_json(ReachAnalyzer::new().with_sections(sections));
        assert_eq!(json["sections"], serde_json::json!(expected));
        for key in ["untested", "test_only", "unreachable"] {
            assert_eq!(json.get(key).is_some(), expected.contains(&key), "{key}");
        }
    }

    #[test]
    fn options_reach_every_section() {
        let opts = ReachOptions {
            top: Some(1),
            section: vec![ReachSection::TestOnly],
            tier: Some(Tier::Unknown),
        };
        let json = analyze_json(ReachAnalyzer::new().with_options(opts));
        assert_eq!(json["sections"], serde_json::json!(["test_only"]));
    }

    #[test]
    fn markdown_nests_every_section_under_one_title() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        let md = ReachAnalyzer::new()
            .analyze(file.as_path(), OutputFormat::Md)
            .unwrap();
        assert!(
            md.starts_with("# Reach (untested, test-only, unreachable)\n"),
            "{md}"
        );
        assert_eq!(
            md.lines().filter(|l| l.starts_with("# ")).count(),
            1,
            "{md}"
        );
        for heading in [
            "## Untested functions",
            "## Test-only report",
            "## Unreachable functions",
        ] {
            assert!(md.contains(heading), "missing {heading}:\n{md}");
        }
    }
}

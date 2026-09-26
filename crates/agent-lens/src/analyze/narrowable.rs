//! `analyze narrowable` — declarations wider than their uses.
//!
//! Four ways a declaration can promise more than anything asks of it,
//! each a section and a full analyzer report in its own right:
//!
//! * `single-use` — a function with one resolved production caller, an
//!   inline candidate;
//! * `single-impl` — a trait or interface with at most one production
//!   implementor, a candidate for the concrete type;
//! * `parameters` — a parameter every call site passes the same value,
//!   or one the body never reads;
//! * `visibility` — a `pub` / exported function whose callers all sit in
//!   a narrower scope.
//!
//! The sections share one call graph and the per-file facts behind it
//! through the analysis index. `--section` narrows the report; the JSON
//! nests each section's report under its key (`single_use`,
//! `single_impl`, `parameters`, `visibility`) and lists the keys it
//! carries in `sections`.
//!
//! # Schema history
//!
//! * `schema_version: 1` — initial shape.

use super::composite::{BundleSection, run_bundle};
use super::options::analyzer_options;
use super::parameters::{ParametersAnalyzer, ParametersOptions};
use super::single_impl::{SingleImplAnalyzer, SingleImplOptions};
use super::single_use::{SingleUseAnalyzer, SingleUseOptions};
use super::visibility::{VisibilityAnalyzer, VisibilityOptions};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

const NOTE: &str = "Declarations that promise more than anything in the analyzed tree asks of \
     them: a function one caller needs, an abstraction one type implements, a parameter that \
     only ever gets one value, a visibility wider than its callers. Every row is a candidate \
     edit, not a verdict; each section states what its graph could not see.";

/// One way a declaration can be wider than its uses. Declaration order
/// is the report order.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum, serde::Deserialize,
)]
#[serde(rename_all = "kebab-case")]
pub enum NarrowableSection {
    /// Functions with exactly one resolved production caller.
    SingleUse,
    /// Traits and interfaces with at most one production implementor.
    SingleImpl,
    /// Constant arguments and dead parameters.
    Parameters,
    /// `pub` / exported functions no caller outside a narrower scope uses.
    Visibility,
}

impl BundleSection for NarrowableSection {
    const ALL: &'static [Self] = &[
        Self::SingleUse,
        Self::SingleImpl,
        Self::Parameters,
        Self::Visibility,
    ];

    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Self::SingleUse => ("single_use", "single-use"),
            Self::SingleImpl => ("single_impl", "single-impl"),
            Self::Parameters => ("parameters", "parameters"),
            Self::Visibility => ("visibility", "visibility"),
        }
    }
}

analyzer_options! {
    /// `analyze narrowable` flags, and the `[profile.<name>.narrowable]`
    /// table.
    pub struct NarrowableOptions {
        @shared(ranking);
        /// Sections to report, comma-separated or repeated:
        /// `single-use`, `single-impl`, `parameters`, `visibility`.
        /// Defaults to all four; they are always reported in that order.
        #[arg(long, value_enum, value_delimiter = ',', value_name = "SECTION")]
        pub section: Vec<NarrowableSection>,
        /// `single-use` body-size ceiling in source lines: a
        /// single-caller function larger than this is excluded from the
        /// candidate list (the calibration section still counts it).
        /// Defaults to 30.
        #[arg(long, value_name = "LINES")]
        pub max_loc: Option<usize>,
        /// `single-use` cyclomatic-complexity ceiling: a single-caller
        /// function branching more than this is excluded from the
        /// candidate list (the calibration section still counts it).
        /// Defaults to 6.
        #[arg(long, value_name = "N")]
        pub max_cyclomatic: Option<u32>,
        /// `parameters` floor: minimum resolved production call sites a
        /// parameter needs before "always the same value" is reported.
        /// Defaults to 2: a single-caller function's arguments are the
        /// `single-use` section's finding, not this one's.
        #[arg(long, value_name = "N")]
        pub min_call_sites: Option<usize>,
    }
}

/// Analyzer entry point for `analyze narrowable`.
#[derive(Debug, Default, Clone)]
pub struct NarrowableAnalyzer {
    single_use: SingleUseAnalyzer,
    single_impl: SingleImplAnalyzer,
    parameters: ParametersAnalyzer,
    visibility: VisibilityAnalyzer,
    sections: Vec<NarrowableSection>,
}

/// Apply the same builder call to every section analyzer.
macro_rules! each_section {
    ($self:ident.$method:ident($arg:expr)) => {
        Self {
            single_use: $self.single_use.$method($arg.clone()),
            single_impl: $self.single_impl.$method($arg.clone()),
            parameters: $self.parameters.$method($arg.clone()),
            visibility: $self.visibility.$method($arg),
            sections: $self.sections,
        }
    };
}

impl NarrowableAnalyzer {
    /// Apply a whole [`NarrowableOptions`] group. The CLI flags and the
    /// `[profile.<name>.narrowable]` table are the same type, so this is
    /// the only seam between parsed options and the analyzer.
    pub fn with_options(self, opts: NarrowableOptions) -> Self {
        let NarrowableOptions {
            top,
            section,
            max_loc,
            max_cyclomatic,
            min_call_sites,
        } = opts;
        Self {
            single_use: self.single_use.with_options(SingleUseOptions {
                top,
                max_loc,
                max_cyclomatic,
            }),
            single_impl: self.single_impl.with_options(SingleImplOptions { top }),
            parameters: self.parameters.with_options(ParametersOptions {
                top,
                min_call_sites,
            }),
            visibility: self.visibility.with_options(VisibilityOptions { top }),
            sections: section,
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Analyze the test corpus instead: each section then treats test
    /// callers as the production callers.
    pub fn with_only_tests(self, only_tests: bool) -> Self {
        each_section!(self.with_only_tests(only_tests))
    }

    /// Drop test-like files. Test callers stop being visible, so the
    /// sections that report a test seam cannot see one.
    pub fn with_exclude_tests(self, exclude_tests: bool) -> Self {
        each_section!(self.with_exclude_tests(exclude_tests))
    }

    pub fn with_exclude_patterns(self, exclude: Vec<String>) -> Self {
        each_section!(self.with_exclude_patterns(exclude))
    }

    /// Restrict the report to these sections; empty means all of them.
    pub fn with_sections(mut self, sections: Vec<NarrowableSection>) -> Self {
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
            "Narrowable",
            NOTE,
            &self.sections,
            format,
            |section| match section {
                NarrowableSection::SingleUse => self.single_use.analyze(&roots, format),
                NarrowableSection::SingleImpl => self.single_impl.analyze(&roots, format),
                NarrowableSection::Parameters => self.parameters.analyze(&roots, format),
                NarrowableSection::Visibility => self.visibility.analyze(&roots, format),
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

    /// `helper` has one caller and always gets `3`; `Shape` has one
    /// implementor; `exposed` is `pub` yet only its own module calls it.
    const FIXTURE: &str = r#"
pub fn main() {
    let a = helper(3);
    let b = other(3);
    let c = other(3);
    exposed();
    println!("{a}{b}{c}");
}

fn helper(x: i32) -> i32 {
    x + 1
}

fn other(x: i32) -> i32 {
    x * 2
}

pub mod inner {
    pub fn exposed() {}

    pub fn run() {
        exposed();
    }
}

use inner::exposed;

trait Shape {
    fn area(&self) -> f64;
}

struct Square;

impl Shape for Square {
    fn area(&self) -> f64 {
        1.0
    }
}
"#;

    fn analyze(analyzer: &NarrowableAnalyzer, format: OutputFormat) -> String {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        analyzer.analyze(file.as_path(), format).unwrap()
    }

    fn analyze_json(analyzer: &NarrowableAnalyzer) -> Value {
        serde_json::from_str(&analyze(analyzer, OutputFormat::Json)).unwrap()
    }

    #[test]
    fn sections_match_the_standalone_analyzers() {
        let dir = tempfile::tempdir().unwrap();
        let file = write_file(dir.path(), "lib.rs", FIXTURE);
        let json: Value = serde_json::from_str(
            &NarrowableAnalyzer::new()
                .analyze(file.as_path(), OutputFormat::Json)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            json["sections"],
            serde_json::json!(["single_use", "single_impl", "parameters", "visibility"])
        );
        let standalone = |body: Result<String, AnalyzerError>| {
            serde_json::from_str::<Value>(&body.unwrap()).unwrap()
        };
        let path = file.as_path();
        assert_eq!(
            json["single_use"],
            standalone(SingleUseAnalyzer::new().analyze(path, OutputFormat::Json))
        );
        assert_eq!(
            json["single_impl"],
            standalone(SingleImplAnalyzer::new().analyze(path, OutputFormat::Json))
        );
        assert_eq!(
            json["parameters"],
            standalone(ParametersAnalyzer::new().analyze(path, OutputFormat::Json))
        );
        assert_eq!(
            json["visibility"],
            standalone(VisibilityAnalyzer::new().analyze(path, OutputFormat::Json))
        );
    }

    #[test]
    fn each_section_finds_its_own_row() {
        let json = analyze_json(&NarrowableAnalyzer::new());
        assert!(json["single_use"].to_string().contains("helper"));
        assert!(json["single_impl"].to_string().contains("Shape"));
        assert!(json["parameters"].to_string().contains("other"));
    }

    #[rstest]
    #[case(vec![NarrowableSection::Visibility], &["visibility"])]
    #[case(
        vec![NarrowableSection::Parameters, NarrowableSection::SingleUse],
        &["single_use", "parameters"],
    )]
    fn section_selection_is_ordered(
        #[case] sections: Vec<NarrowableSection>,
        #[case] expected: &[&str],
    ) {
        let json = analyze_json(&NarrowableAnalyzer::new().with_sections(sections));
        assert_eq!(json["sections"], serde_json::json!(expected));
        for section in NarrowableSection::ALL {
            let (key, _) = section.labels();
            assert_eq!(json.get(key).is_some(), expected.contains(&key), "{key}");
        }
    }

    /// Each option lands on the one section it belongs to: a
    /// `max-loc` of 0 empties the single-use candidates, and a
    /// `min-call-sites` of 3 drops the two-site constant argument.
    #[test]
    fn options_reach_their_sections() {
        let opts = NarrowableOptions {
            max_loc: Some(0),
            min_call_sites: Some(3),
            ..NarrowableOptions::default()
        };
        let json = analyze_json(&NarrowableAnalyzer::new().with_options(opts));
        assert_eq!(json["single_use"]["thresholds"]["max_loc"], 0);
        assert_eq!(json["parameters"]["thresholds"]["min_call_sites"], 3);
    }

    #[test]
    fn markdown_nests_every_section_under_one_title() {
        let md = analyze(&NarrowableAnalyzer::new(), OutputFormat::Md);
        assert!(
            md.starts_with("# Narrowable (single-use, single-impl, parameters, visibility)\n"),
            "{md}"
        );
        assert_eq!(
            md.lines().filter(|l| l.starts_with("# ")).count(),
            1,
            "{md}"
        );
        for heading in [
            "## Single-use report",
            "## Single-impl report",
            "## Parameters report",
            "## Over-exposed visibility",
        ] {
            assert!(md.contains(heading), "missing {heading}:\n{md}");
        }
    }
}

//! `analyze graph` — the static function call graph, dumped whole or
//! answered one traversal at a time.
//!
//! Without `--query` the report is the graph itself, as
//! visualization-ready JSON ([`super::function_graph`]). With `--query`
//! it is one canned traversal over the same graph ([`super::graph_query`]).
//! The two were separate analyzers over one substrate; the flag that
//! picks a traversal is now the only thing between them, and each mode's
//! report is unchanged.

use super::function_graph::FunctionGraphAnalyzer;
use super::graph_query::{GraphDirection, GraphQueryAnalyzer, GraphQueryKind};
use super::{AnalyzeRoots, AnalyzerError, OutputFormat};

/// `analyze graph` flags, and the `[profile.<name>.graph]` table.
///
/// Every key is optional: an absent `query` dumps the graph, and the
/// traversal keys only mean something next to one. Written out rather
/// than generated so the traversal keys can say so on the command line.
#[derive(Debug, Clone, Default, clap::Args, serde::Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
pub struct GraphOptions {
    /// Traversal verb to run. Omit it to emit the whole graph instead.
    #[arg(long, value_enum)]
    pub query: Option<GraphQueryKind>,
    /// Function to start from, required with `--query`: a
    /// `::`-segment suffix of its qualified name (e.g. `foo`,
    /// `module::foo`, `Owner::method`) or an exact node id
    /// (`file:name:line`, as listed on ambiguity).
    #[arg(long, requires = "query")]
    pub symbol: Option<String>,
    /// Destination symbol for the `path` query (same matching rules as
    /// the start symbol).
    #[arg(long, requires = "query")]
    pub to: Option<String>,
    /// Traversal depth cap in call hops. Defaults to 1 for
    /// callers/callees/neighborhood; for `path` it caps the search
    /// (default unbounded).
    #[arg(long, requires = "query")]
    pub depth: Option<usize>,
    /// Traversal direction for the `neighborhood` query (default both).
    #[arg(long, value_enum, requires = "query")]
    pub direction: Option<GraphDirection>,
    /// Cap the traversal's result set by node count (default 50).
    #[arg(long, requires = "query")]
    pub limit: Option<usize>,
}

impl GraphOptions {
    /// Why these options cannot run, if they cannot. Shared by the
    /// analyzer and profile validation, so a `[profile.<name>.graph]`
    /// table fails at load time with the message a run would give.
    pub fn problem(&self) -> Option<&'static str> {
        match self.query {
            Some(_) if self.symbol.is_none() => {
                Some("`query` requires `symbol`, the function to start from")
            }
            Some(_) => None,
            None if self.symbol.is_some()
                || self.to.is_some()
                || self.depth.is_some()
                || self.direction.is_some()
                || self.limit.is_some() =>
            {
                Some(
                    "`symbol`, `to`, `depth`, `direction` and `limit` only apply with `query`; \
                     without it the whole graph is emitted",
                )
            }
            None => None,
        }
    }
}

/// Analyzer entry point for `analyze graph`.
#[derive(Debug, Clone, Default)]
pub struct GraphAnalyzer {
    opts: GraphOptions,
    only_tests: bool,
    exclude_tests: bool,
    exclude: Vec<String>,
}

impl GraphAnalyzer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply a whole [`GraphOptions`] group. The CLI flags and the
    /// `[profile.<name>.graph]` table are the same type, so this is the
    /// only seam between parsed options and the analyzer.
    pub fn with_options(mut self, opts: GraphOptions) -> Self {
        self.opts = opts;
        self
    }

    pub fn with_only_tests(mut self, only_tests: bool) -> Self {
        self.only_tests = only_tests;
        self
    }

    pub fn with_exclude_tests(mut self, exclude_tests: bool) -> Self {
        self.exclude_tests = exclude_tests;
        self
    }

    pub fn with_exclude_patterns(mut self, exclude: Vec<String>) -> Self {
        self.exclude = exclude;
        self
    }

    pub fn analyze(
        &self,
        roots: impl Into<AnalyzeRoots>,
        format: OutputFormat,
    ) -> Result<String, AnalyzerError> {
        if let Some(message) = self.opts.problem() {
            return Err(AnalyzerError::InvalidQuery {
                message: message.to_owned(),
            });
        }
        let (Some(query), Some(symbol)) = (self.opts.query, self.opts.symbol.clone()) else {
            return FunctionGraphAnalyzer::new()
                .with_only_tests(self.only_tests)
                .with_exclude_tests(self.exclude_tests)
                .with_exclude_patterns(self.exclude.clone())
                .analyze(roots, format);
        };
        GraphQueryAnalyzer::new(query, symbol)
            .with_to(self.opts.to.clone())
            .with_depth(self.opts.depth)
            .with_direction(self.opts.direction)
            .with_limit(self.opts.limit)
            .with_only_tests(self.only_tests)
            .with_exclude_tests(self.exclude_tests)
            .with_exclude_patterns(self.exclude.clone())
            .analyze(roots, format)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;
    use crate::test_support::write_file;

    const SOURCE: &str = "pub fn sink() {}\npub fn caller() { sink(); }\n";

    /// Each mode is exactly the report its former analyzer produced.
    #[rstest]
    #[case::dump_json(OutputFormat::Json, None)]
    #[case::dump_md(OutputFormat::Md, None)]
    #[case::query_json(OutputFormat::Json, Some(GraphQueryKind::Callers))]
    #[case::query_md(OutputFormat::Md, Some(GraphQueryKind::Callees))]
    fn each_mode_is_its_former_report(
        #[case] format: OutputFormat,
        #[case] query: Option<GraphQueryKind>,
    ) {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lib.rs", SOURCE);
        let opts = GraphOptions {
            query,
            symbol: query.map(|_| "sink".to_owned()),
            depth: query.map(|_| 2),
            ..GraphOptions::default()
        };
        let merged = GraphAnalyzer::new()
            .with_options(opts)
            .with_exclude_tests(true)
            .analyze(dir.path(), format)
            .unwrap();
        let former = match query {
            None => FunctionGraphAnalyzer::new()
                .with_exclude_tests(true)
                .analyze(dir.path(), format),
            Some(query) => GraphQueryAnalyzer::new(query, "sink")
                .with_depth(Some(2))
                .with_exclude_tests(true)
                .analyze(dir.path(), format),
        }
        .unwrap();
        assert_eq!(merged, former);
    }

    #[rstest]
    #[case::query_without_symbol(
        GraphOptions { query: Some(GraphQueryKind::Callers), ..GraphOptions::default() },
        "requires `symbol`",
    )]
    #[case::symbol_without_query(
        GraphOptions { symbol: Some("sink".to_owned()), ..GraphOptions::default() },
        "only apply with `query`",
    )]
    #[case::limit_without_query(
        GraphOptions { limit: Some(3), ..GraphOptions::default() },
        "only apply with `query`",
    )]
    #[case::to_without_query(
        GraphOptions { to: Some("sink".to_owned()), ..GraphOptions::default() },
        "only apply with `query`",
    )]
    #[case::depth_without_query(
        GraphOptions { depth: Some(2), ..GraphOptions::default() },
        "only apply with `query`",
    )]
    #[case::direction_without_query(
        GraphOptions { direction: Some(GraphDirection::In), ..GraphOptions::default() },
        "only apply with `query`",
    )]
    fn options_that_cannot_run_are_rejected(#[case] opts: GraphOptions, #[case] expected: &str) {
        assert!(opts.problem().is_some_and(|m| m.contains(expected)));
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "lib.rs", SOURCE);
        let err = GraphAnalyzer::new()
            .with_options(opts)
            .analyze(dir.path(), OutputFormat::Json)
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
    }

    #[test]
    fn default_options_have_no_problem() {
        assert_eq!(GraphOptions::default().problem(), None);
    }
}

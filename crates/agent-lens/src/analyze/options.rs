//! Declaring an analyzer's option surface once.
//!
//! Every analyzer option used to be spelled three times: a clap `Args`
//! struct in the CLI, a serde `Options` struct in [`crate::config`], and a
//! hand-written field-by-field copier between them. The types built here
//! are both at once — `clap::Args` supplies the flags, `Deserialize`
//! supplies the `[profile.<name>.<tool>]` table — so a profile entry *is*
//! the value the CLI would have parsed and no conversion is needed.
//!
//! Each analyzer owns its own options type, next to the builder that
//! consumes it, so adding an analyzer touches one module instead of three.
//!
//! `deny_unknown_fields` stays on every tool table: an option set on the
//! wrong tool must be a parse error, not a silent no-op. That rules out
//! `#[serde(flatten)]` for the options shared across analyzers (serde
//! rejects the combination, and the [`crate::config_schema`] reflector
//! cannot see through a flattened map either), so [`analyzer_options`]
//! expands the shared fields — with their documentation — into each struct
//! that opts in. The shared spellings therefore exist once: `--top` and
//! `--diff-only` cannot drift apart between analyzers.

/// Declare an analyzer's options as a single clap-and-serde type.
///
/// The body opens with `@shared(...)` naming which cross-analyzer options
/// to include, followed by the analyzer's own fields:
///
/// ```ignore
/// analyzer_options! {
///     /// `[profile.<name>.cohesion]` overrides.
///     pub struct CohesionOptions {
///         @shared(ranking, diff);
///         /// Minimum LCOM4 score included in the markdown ranking.
///         #[arg(long)]
///         pub min_score: Option<usize>,
///     }
/// }
/// ```
///
/// `ranking` expands to `top`, `diff` to `diff_only`. Omit the
/// `@shared(...)` line for an analyzer that takes neither.
///
/// The generated type derives `Default`, which `#[serde(default)]` uses to
/// fill an absent table. An analyzer whose flags carry non-trivial clap
/// defaults (`similarity`) or required keys (`graph-query`) is written out
/// by hand instead, so its `Default` and its `default_value_t` cannot
/// disagree.
macro_rules! analyzer_options {
    // Internal: attach the derives shared by every options type. Listed
    // first so the `@emit` token is never swallowed by the public arms.
    (
        @emit
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { $($body:tt)* }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Default, ::clap::Args, ::serde::Deserialize)]
        #[serde(rename_all = "kebab-case", deny_unknown_fields, default)]
        $vis struct $name { $($body)* }
    };

    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { @shared(ranking, diff); $($rest:tt)* }
    ) => {
        analyzer_options! {
            @emit
            $(#[$meta])*
            $vis struct $name {
                /// Cap the markdown ranking to the top-N entries. JSON
                /// output always carries the full list.
                #[arg(long)]
                pub top: Option<usize>,
                /// Restrict the report to units touching unstaged changed
                /// lines in `git diff -U0`.
                #[arg(long)]
                pub diff_only: bool,
                $($rest)*
            }
        }
    };

    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { @shared(ranking); $($rest:tt)* }
    ) => {
        analyzer_options! {
            @emit
            $(#[$meta])*
            $vis struct $name {
                /// Cap the markdown ranking to the top-N entries. JSON
                /// output always carries the full list.
                #[arg(long)]
                pub top: Option<usize>,
                $($rest)*
            }
        }
    };

    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { @shared(diff); $($rest:tt)* }
    ) => {
        analyzer_options! {
            @emit
            $(#[$meta])*
            $vis struct $name {
                /// Restrict the report to units touching unstaged changed
                /// lines in `git diff -U0`.
                #[arg(long)]
                pub diff_only: bool,
                $($rest)*
            }
        }
    };

    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { $($rest:tt)* }
    ) => {
        analyzer_options! {
            @emit
            $(#[$meta])*
            $vis struct $name { $($rest)* }
        }
    };
}

pub(crate) use analyzer_options;

#[cfg(test)]
mod tests {
    use crate::analyze::cohesion::{CohesionAnalyzer, CohesionOptions};
    use crate::analyze::complexity::{ComplexityAnalyzer, ComplexityOptions};
    use crate::analyze::context_span::{ContextSpanAnalyzer, ContextSpanOptions};
    use crate::analyze::delegation::{DelegationAnalyzer, DelegationOptions};
    use crate::analyze::graph_query::{
        GraphDirection, GraphQueryAnalyzer, GraphQueryKind, GraphQueryOptions,
    };
    use crate::analyze::hotspot::{HotspotAnalyzer, HotspotOptions};
    use crate::analyze::hubs::{HubsAnalyzer, HubsOptions};
    use crate::analyze::impact::{ImpactAnalyzer, ImpactOptions};
    use crate::analyze::layers::{LayersAnalyzer, LayersOptions};
    use crate::analyze::risk::{RiskAnalyzer, RiskOptions};
    use crate::analyze::similarity::{SimilarityAnalyzer, SimilarityOptions};
    use crate::analyze::unreachable::{Tier, UnreachableAnalyzer, UnreachableOptions};
    use crate::analyze::untested::{UntestedAnalyzer, UntestedOptions};
    use crate::analyze::visibility::{VisibilityAnalyzer, VisibilityOptions};
    use crate::analyze::wrapper::{WrapperAnalyzer, WrapperOptions};

    /// `with_options` is the single seam between parsed options and an
    /// analyzer, so a field silently dropped there disables a flag on
    /// both the CLI and the config file at once, with nothing else to
    /// catch it.
    ///
    /// Each case asserts two things about one analyzer: that
    /// `with_options` lands in exactly the state its explicit builder
    /// chain would (a dropped field shows up as a differing rendering),
    /// and that the state differs from an unconfigured analyzer — without
    /// the second half a `with_options` that ignored its argument
    /// entirely would still pass. The analyzers keep their configuration
    /// private, so comparing `Debug` renderings is the cheapest check
    /// that covers every field at once.
    macro_rules! assert_options_reach_the_analyzer {
        ($name:ident: $analyzer:ty, $opts:expr, |$binding:ident| $chain:expr) => {
            #[test]
            fn $name() {
                let opts = $opts;
                let via_options = <$analyzer>::new().with_options(opts);
                let $binding = <$analyzer>::new();
                let via_builders: $analyzer = $chain;
                assert_eq!(
                    format!("{via_options:?}"),
                    format!("{via_builders:?}"),
                    "with_options must equal the builder chain it stands in for",
                );
                assert_ne!(
                    format!("{via_options:?}"),
                    format!("{:?}", <$analyzer>::new()),
                    "the options must actually reach the analyzer",
                );
            }
        };
    }

    assert_options_reach_the_analyzer!(
        cohesion_options_reach_the_analyzer: CohesionAnalyzer,
        CohesionOptions { top: Some(3), diff_only: true, min_score: Some(4) },
        |a| a.with_top(Some(3)).with_min_score(Some(4)).with_diff_only(true)
    );
    assert_options_reach_the_analyzer!(
        complexity_options_reach_the_analyzer: ComplexityAnalyzer,
        ComplexityOptions { top: Some(3), diff_only: true, min_score: Some(4) },
        |a| a.with_top(Some(3)).with_min_score(Some(4)).with_diff_only(true)
    );
    assert_options_reach_the_analyzer!(
        context_span_options_reach_the_analyzer: ContextSpanAnalyzer,
        ContextSpanOptions { entry_glob: vec!["app/**/page.tsx".to_owned()] },
        |a| a.with_entry_globs(vec!["app/**/page.tsx".to_owned()])
    );
    assert_options_reach_the_analyzer!(
        delegation_options_reach_the_analyzer: DelegationAnalyzer,
        DelegationOptions { top: Some(3), diff_only: true },
        |a| a.with_top(Some(3)).with_diff_only(true)
    );
    assert_options_reach_the_analyzer!(
        hotspot_options_reach_the_analyzer: HotspotAnalyzer,
        HotspotOptions { top: Some(3), since: Some("90.days.ago".to_owned()) },
        |a| a.with_top(Some(3)).with_since_opt(Some("90.days.ago".to_owned()))
    );
    assert_options_reach_the_analyzer!(
        hubs_options_reach_the_analyzer: HubsAnalyzer,
        HubsOptions { top: Some(3) },
        |a| a.with_top(Some(3))
    );
    assert_options_reach_the_analyzer!(
        impact_options_reach_the_analyzer: ImpactAnalyzer,
        ImpactOptions { top: Some(3), function: vec!["resolve".to_owned()], depth: Some(2) },
        |a| a
            .with_functions(vec!["resolve".to_owned()])
            .with_depth(Some(2))
            .with_top(Some(3))
    );
    assert_options_reach_the_analyzer!(
        layers_options_reach_the_analyzer: LayersAnalyzer,
        LayersOptions { top: Some(3) },
        |a| a.with_top(Some(3))
    );
    assert_options_reach_the_analyzer!(
        risk_options_reach_the_analyzer: RiskAnalyzer,
        RiskOptions { top: Some(3), since: Some("90.days.ago".to_owned()) },
        |a| a.with_top(Some(3)).with_since_opt(Some("90.days.ago".to_owned()))
    );
    assert_options_reach_the_analyzer!(
        unreachable_options_reach_the_analyzer: UnreachableAnalyzer,
        UnreachableOptions { top: Some(3), tier: Some(Tier::Unknown) },
        |a| a.with_top(Some(3)).with_tier(Some(Tier::Unknown))
    );
    assert_options_reach_the_analyzer!(
        untested_options_reach_the_analyzer: UntestedAnalyzer,
        UntestedOptions { top: Some(3) },
        |a| a.with_top(Some(3))
    );
    assert_options_reach_the_analyzer!(
        visibility_options_reach_the_analyzer: VisibilityAnalyzer,
        VisibilityOptions { top: Some(3) },
        |a| a.with_top(Some(3))
    );
    assert_options_reach_the_analyzer!(
        wrapper_options_reach_the_analyzer: WrapperAnalyzer,
        WrapperOptions { diff_only: true },
        |a| a.with_diff_only(true)
    );

    /// Similarity is hand-written rather than generated, and its `sweep`
    /// needs the empty-ladder-means-no-sweep conversion, so it gets the
    /// same treatment with every field set away from its default.
    #[test]
    fn similarity_options_reach_the_analyzer() {
        let opts = SimilarityOptions {
            top: Some(3),
            diff_only: true,
            threshold: 0.6,
            sweep: vec![0.6, 0.8],
            paired_by: None,
            drift_floor: 0.4,
            min_lines: Some(9),
            target: crate::analyze::SimilarityTarget::Types,
            method: crate::analyze::SimilarityMethod::Token,
            doc_overlap: true,
        };
        let via_options = SimilarityAnalyzer::new().with_options(opts);
        let via_builders = SimilarityAnalyzer::new()
            .with_threshold(0.6)
            .with_sweep(Some(vec![0.6, 0.8]))
            .with_diff_only(true)
            .with_min_lines_opt(Some(9))
            .with_target(crate::analyze::SimilarityTarget::Types)
            .with_method(crate::analyze::SimilarityMethod::Token)
            .with_doc_overlap(true)
            .with_paired_by(None)
            .with_drift_floor(0.4)
            .with_top(Some(3));
        assert_eq!(format!("{via_options:?}"), format!("{via_builders:?}"));
        assert_ne!(
            format!("{via_options:?}"),
            format!("{:?}", SimilarityAnalyzer::new()),
        );
    }

    /// An empty `sweep` is the flag's absence, not a zero-rung ladder.
    #[test]
    fn similarity_empty_sweep_means_no_sweep() {
        let via_options = SimilarityAnalyzer::new().with_options(SimilarityOptions::default());
        let via_builders = SimilarityAnalyzer::new().with_sweep(None);
        assert_eq!(format!("{via_options:?}"), format!("{via_builders:?}"));
    }

    /// `graph-query` constructs rather than configures: its `query` and
    /// `symbol` are required, so it has `from_options` instead.
    #[test]
    fn graph_query_options_reach_the_analyzer() {
        let opts = GraphQueryOptions {
            query: GraphQueryKind::Path,
            symbol: "handler".to_owned(),
            to: Some("db_write".to_owned()),
            depth: Some(3),
            direction: Some(GraphDirection::In),
            limit: Some(10),
        };
        let via_options = GraphQueryAnalyzer::from_options(opts);
        let via_builders = GraphQueryAnalyzer::new(GraphQueryKind::Path, "handler")
            .with_to(Some("db_write".to_owned()))
            .with_depth(Some(3))
            .with_direction(Some(GraphDirection::In))
            .with_limit(Some(10));
        assert_eq!(format!("{via_options:?}"), format!("{via_builders:?}"));
        assert_ne!(
            format!("{via_options:?}"),
            format!(
                "{:?}",
                GraphQueryAnalyzer::new(GraphQueryKind::Path, "handler")
            ),
        );
    }
}

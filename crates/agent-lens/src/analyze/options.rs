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

//! Per-analyzer adapters that turn the typed `collect()` output of an
//! analyzer into the analyzer-agnostic [`Item`](super::Item) shape the
//! baseline subsystem stores and compares.
//!
//! Each adapter implements the [`Baselinable`](super::Baselinable)
//! trait for compile-time defaults (metric set, primary metric,
//! new-item threshold) and exposes free functions the runner uses to
//! build a [`Snapshot`](super::Snapshot) and a
//! [`RatchetConfig`](super::compare::RatchetConfig).

pub mod cohesion;
pub mod complexity;
pub mod coupling;
pub mod hotspot;

use super::compare::RatchetConfig;
use super::{Baselinable, BaselineError, NewItemPolicy};

/// Validate a user-supplied `--metric` list against the analyzer's
/// known metric set, then assemble a [`RatchetConfig`]. Empty input
/// falls back to the analyzer's `ratchet_metrics()` defaults.
///
/// Centralised here so every adapter's `ratchet_config` builder uses
/// the same validation path and produces the same `UnknownMetric`
/// diagnostic.
pub fn build_ratchet_config<B: Baselinable>(
    metrics_override: Vec<String>,
    policy: NewItemPolicy,
) -> Result<RatchetConfig, BaselineError> {
    let known: Vec<&'static str> = B::ratchet_metrics().to_vec();
    let metrics = if metrics_override.is_empty() {
        known.iter().map(|s| (*s).to_owned()).collect()
    } else {
        for m in &metrics_override {
            if !known.contains(&m.as_str()) {
                return Err(BaselineError::UnknownMetric {
                    analyzer: B::ANALYZER_NAME.to_owned(),
                    requested: m.clone(),
                    known: known.join(", "),
                });
            }
        }
        metrics_override
    };
    Ok(RatchetConfig {
        metrics,
        primary_metric: B::primary_metric().to_owned(),
        new_item_threshold: B::new_item_threshold(B::primary_metric()),
        policy,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baseline::Baselinable;

    struct Dummy;
    impl Baselinable for Dummy {
        const ANALYZER_NAME: &'static str = "dummy";
        fn ratchet_metrics() -> &'static [&'static str] {
            &["alpha", "beta"]
        }
        fn primary_metric() -> &'static str {
            "alpha"
        }
        fn new_item_threshold(metric: &str) -> Option<f64> {
            (metric == "alpha").then_some(7.0)
        }
    }

    #[test]
    fn empty_override_uses_default_metric_set() {
        let cfg = build_ratchet_config::<Dummy>(Vec::new(), NewItemPolicy::Strict).unwrap();
        assert_eq!(cfg.metrics, vec!["alpha", "beta"]);
        assert_eq!(cfg.primary_metric, "alpha");
        assert_eq!(cfg.new_item_threshold, Some(7.0));
    }

    #[test]
    fn override_with_known_metrics_is_kept() {
        let cfg =
            build_ratchet_config::<Dummy>(vec!["beta".to_owned()], NewItemPolicy::Strict).unwrap();
        assert_eq!(cfg.metrics, vec!["beta"]);
    }

    #[test]
    fn override_with_unknown_metric_errors_with_known_list() {
        let err = build_ratchet_config::<Dummy>(vec!["gamma".to_owned()], NewItemPolicy::Strict)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("gamma"), "got: {msg}");
        assert!(msg.contains("alpha"), "got: {msg}");
        assert!(msg.contains("beta"), "got: {msg}");
    }
}

//! Signature-level similarity between two functions.
//!
//! The signature half of a `similarity` pair score: identifier, type,
//! generic and receiver overlap of two [`SignatureShape`]s. The body
//! half comes from [`crate::tsed`], [`crate::token_similarity`] or
//! [`crate::pdg`]; the caller blends the two.

use std::collections::HashSet;

use crate::SignatureShape;

/// Signature-level similarity of two functions, each component in
/// `[0.0, 1.0]`. Every field is `None` when either side has no signature
/// (type definitions and statement blocks).
#[derive(Debug, Clone, Copy)]
pub struct SignatureComponents {
    pub signature_similarity: Option<f64>,
    pub type_overlap: Option<f64>,
    pub identifier_overlap: Option<f64>,
}

/// Score two signatures on names, parameter and return types, generics
/// and receiver shape. Type paths carry the most weight: two functions
/// over the same domain types are likelier duplicates than two with the
/// same body over different types.
pub fn signature_components(
    a: Option<&SignatureShape>,
    b: Option<&SignatureShape>,
) -> SignatureComponents {
    let (Some(a), Some(b)) = (a, b) else {
        return SignatureComponents {
            signature_similarity: None,
            type_overlap: None,
            identifier_overlap: None,
        };
    };

    let identifier_overlap = token_overlap(
        a.name_tokens().chain(a.parameter_names()),
        b.name_tokens().chain(b.parameter_names()),
    );
    let type_overlap = token_overlap(
        a.parameter_type_paths()
            .chain(a.return_type_paths.iter().map(String::as_str)),
        b.parameter_type_paths()
            .chain(b.return_type_paths.iter().map(String::as_str)),
    );
    let parameter_name_overlap = token_overlap(a.parameter_names(), b.parameter_names());
    let generic_overlap = token_overlap(a.generics(), b.generics());
    let parameter_count = count_similarity(a.parameter_count(), b.parameter_count());
    let receiver = if a.receiver_shape() == b.receiver_shape() {
        1.0
    } else {
        0.0
    };
    let signature_similarity = (0.25 * identifier_overlap)
        + (0.10 * parameter_count)
        + (0.05 * parameter_name_overlap)
        + (0.45 * type_overlap)
        + (0.10 * generic_overlap)
        + (0.05 * receiver);

    SignatureComponents {
        signature_similarity: Some(signature_similarity),
        type_overlap: Some(type_overlap),
        identifier_overlap: Some(identifier_overlap),
    }
}

/// Jaccard overlap of two token sets. Two empty sets are identical.
pub fn token_overlap<'a>(
    a: impl Iterator<Item = &'a str>,
    b: impl Iterator<Item = &'a str>,
) -> f64 {
    let a: HashSet<&str> = a.collect();
    let b: HashSet<&str> = b.collect();
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let intersection = a.intersection(&b).count();
    let union = a.union(&b).count();
    if union == 0 {
        1.0
    } else {
        intersection as f64 / union as f64
    }
}

/// `1 - |a - b| / max(a, b)`, or `1.0` when both are zero.
pub fn count_similarity(a: usize, b: usize) -> f64 {
    let max = a.max(b);
    if max == 0 {
        return 1.0;
    }
    1.0 - (a.abs_diff(b) as f64 / max as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[rstest]
    #[case::empty_against_non_empty(&[], &["user"], 0.0)]
    #[case::both_empty(&[], &[], 1.0)]
    #[case::partial(&["user", "id"], &["id", "order"], 1.0 / 3.0)]
    #[case::identical(&["id"], &["id"], 1.0)]
    fn token_overlap_is_jaccard(#[case] a: &[&str], #[case] b: &[&str], #[case] expected: f64) {
        assert_eq!(
            token_overlap(a.iter().copied(), b.iter().copied()),
            expected
        );
    }

    #[rstest]
    #[case::both_zero(0, 0, 1.0)]
    #[case::half(2, 4, 0.5)]
    #[case::one_zero(0, 3, 0.0)]
    fn count_similarity_is_relative_difference(
        #[case] a: usize,
        #[case] b: usize,
        #[case] expected: f64,
    ) {
        assert_eq!(count_similarity(a, b), expected);
    }

    fn rust_sig(
        name_tokens: &[&str],
        parameter_names: &[&str],
        parameter_type_paths: &[&str],
        return_type_paths: &[&str],
    ) -> crate::SignatureShape {
        crate::FunctionSignature {
            name_tokens: name_tokens.iter().map(|s| (*s).to_owned()).collect(),
            parameter_count: parameter_names.len(),
            parameter_names: parameter_names.iter().map(|s| (*s).to_owned()).collect(),
            parameter_type_paths: parameter_type_paths
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            return_type_paths: return_type_paths.iter().map(|s| (*s).to_owned()).collect(),
            generics: Vec::new(),
            receiver: crate::ReceiverShape::None,
        }
        .into()
    }

    fn rust_sig_with_receiver(
        name_tokens: &[&str],
        parameter_names: &[&str],
        parameter_type_paths: &[&str],
        return_type_paths: &[&str],
        generics: &[&str],
        receiver: crate::ReceiverShape,
    ) -> crate::SignatureShape {
        let mut sig = crate::FunctionSignature {
            name_tokens: name_tokens.iter().map(|s| (*s).to_owned()).collect(),
            parameter_count: parameter_names.len(),
            parameter_names: parameter_names.iter().map(|s| (*s).to_owned()).collect(),
            parameter_type_paths: parameter_type_paths
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            return_type_paths: return_type_paths.iter().map(|s| (*s).to_owned()).collect(),
            generics: Vec::new(),
            receiver: crate::ReceiverShape::None,
        };
        sig.generics = generics.iter().map(|s| (*s).to_owned()).collect();
        sig.receiver = receiver;
        sig.into()
    }

    #[test]
    fn signature_score_rewards_same_domain_types_over_same_body_different_types() {
        let same_domain_renamed = signature_components(
            Some(&rust_sig(&["validate"], &["id"], &["UserId"], &["bool"])),
            Some(&rust_sig(
                &["validate"],
                &["candidate"],
                &["UserId"],
                &["bool"],
            )),
        )
        .signature_similarity
        .unwrap();
        let different_domain_type = signature_components(
            Some(&rust_sig(&["validate"], &["id"], &["UserId"], &["bool"])),
            Some(&rust_sig(&["validate"], &["id"], &["OrderId"], &["bool"])),
        )
        .signature_similarity
        .unwrap();

        assert!(
            same_domain_renamed > different_domain_type,
            "renamed={same_domain_renamed}, different_type={different_domain_type}",
        );
    }

    #[test]
    fn signature_components_calculates_observable_subscores() {
        let left = rust_sig_with_receiver(
            &["get", "user"],
            &["id"],
            &["UserId"],
            &["User"],
            &["T: Clone"],
            crate::ReceiverShape::Ref,
        );
        let right = rust_sig_with_receiver(
            &["get", "order"],
            &["other"],
            &["OrderId"],
            &["Order"],
            &["E: Clone"],
            crate::ReceiverShape::RefMut,
        );

        let score = signature_components(Some(&left), Some(&right));

        assert_eq!(score.identifier_overlap, Some(0.2));
        assert_eq!(score.type_overlap, Some(0.0));
        assert!((score.signature_similarity.unwrap() - 0.15).abs() < 1e-9);

        let same_receiver = rust_sig_with_receiver(
            &["get", "order"],
            &["other"],
            &["OrderId"],
            &["Order"],
            &["E: Clone"],
            crate::ReceiverShape::Ref,
        );
        let with_receiver_match = signature_components(Some(&left), Some(&same_receiver));
        assert!(
            with_receiver_match.signature_similarity.unwrap() > score.signature_similarity.unwrap()
        );

        let different_parameter_count = signature_components(
            Some(&rust_sig(&[], &["id"], &[], &[])),
            Some(&rust_sig(&[], &["id", "fallback"], &[], &[])),
        );
        assert!((different_parameter_count.signature_similarity.unwrap() - 0.8).abs() < 1e-9);
    }
}

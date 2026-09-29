//! Macros that build the mechanical `From<…Error>` impls for the
//! analyzer error enums.

/// Generate `impl From<ChurnError> for $dst`.
///
/// Churn extraction lives in [`super::churn`] so `analyze hotspot` and
/// `analyze risk` share it verbatim, and its failures map one-for-one
/// onto variants both analyzers already expose. The two conversions were
/// identical to the character; keeping the mapping here means a new
/// `ChurnError` variant is one compile error in one place instead of two
/// silently divergent copies.
macro_rules! impl_from_churn_error {
    ($dst:ty) => {
        impl From<$crate::analyze::churn::ChurnError> for $dst {
            fn from(error: $crate::analyze::churn::ChurnError) -> Self {
                use $crate::analyze::churn::ChurnError as Inner;
                match error {
                    Inner::Io { path, source } => Self::Io { path, source },
                    Inner::Git { stderr } => Self::Git { stderr },
                    Inner::NotInGitRepo { path } => Self::NotInGitRepo { path },
                }
            }
        }
    };
}

pub(crate) use impl_from_churn_error;

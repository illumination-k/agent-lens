//! Macros that build the mechanical `From<…Error>` impls for the
//! analyzer error enums.

/// Generate `impl From<lens_domain::CouplingError> for $dst`.
///
/// Every language adapter reports module-tree failures through the one
/// shared [`lens_domain::CouplingError`], and the crate-level analyzer
/// errors mirror its variants field-for-field, so the conversion is a
/// straight copy.
macro_rules! impl_from_coupling_error {
    ($dst:ty) => {
        impl From<::lens_domain::CouplingError> for $dst {
            fn from(value: ::lens_domain::CouplingError) -> Self {
                use ::lens_domain::CouplingError as Inner;
                match value {
                    Inner::Io { path, source } => Self::Io { path, source },
                    Inner::Parse { path, source } => Self::Parse { path, source },
                    Inner::MissingMod { parent, name, near } => {
                        Self::MissingMod { parent, name, near }
                    }
                    Inner::UnsupportedRoot { path } => Self::UnsupportedRoot { path },
                }
            }
        }
    };
}

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

pub(crate) use {impl_from_churn_error, impl_from_coupling_error};

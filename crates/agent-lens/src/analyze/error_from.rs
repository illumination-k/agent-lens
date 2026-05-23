//! Macro that builds `From<lens_*::CouplingError>` impls for the
//! analyzer error enums.
//!
//! Every per-language `CouplingError` carries the same `Io` and `Parse`
//! variants plus an optional subset of `MissingMod` / `UnsupportedRoot`.
//! Targets all share the `Io { path, source }`, `Parse { path, source:
//! Box<dyn Error> }` shape, so the conversion is mechanical: copy `Io`
//! through, box the `Parse` source, and forward whichever optional
//! variants the source enum carries.

/// Generate `impl From<$src> for $dst` matching the per-language
/// `CouplingError` shape. The optional trailing variant name (one of
/// `MissingMod`, `UnsupportedRoot`) forwards that extra variant
/// field-for-field.
macro_rules! impl_from_coupling_error {
    ($src:path => $dst:ty) => {
        impl From<$src> for $dst {
            fn from(value: $src) -> Self {
                use $src as Inner;
                match value {
                    Inner::Io { path, source } => Self::Io { path, source },
                    Inner::Parse { path, source } => Self::Parse {
                        path,
                        source: Box::new(source),
                    },
                }
            }
        }
    };
    ($src:path => $dst:ty, MissingMod) => {
        impl From<$src> for $dst {
            fn from(value: $src) -> Self {
                use $src as Inner;
                match value {
                    Inner::Io { path, source } => Self::Io { path, source },
                    Inner::Parse { path, source } => Self::Parse {
                        path,
                        source: Box::new(source),
                    },
                    Inner::MissingMod { parent, name, near } => {
                        Self::MissingMod { parent, name, near }
                    }
                }
            }
        }
    };
    ($src:path => $dst:ty, UnsupportedRoot) => {
        impl From<$src> for $dst {
            fn from(value: $src) -> Self {
                use $src as Inner;
                match value {
                    Inner::Io { path, source } => Self::Io { path, source },
                    Inner::Parse { path, source } => Self::Parse {
                        path,
                        source: Box::new(source),
                    },
                    Inner::UnsupportedRoot { path } => Self::UnsupportedRoot { path },
                }
            }
        }
    };
}

pub(crate) use impl_from_coupling_error;

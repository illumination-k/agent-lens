//! Thin-wrapper detection for Quint.
//!
//! A definition is a wrapper when its whole value forwards to another
//! user definition with its parameters passed straight through:
//!
//! * `def f(a, b) = g(a, b)` (or `g(b, a)`; the check is set-based), or
//! * `action step = doStep` — a parameterless alias of another
//!   definition, recognised only when the name resolves to one.
//!
//! Built-in operators are never callees: `def size(s) = s.size()` names
//! a language operator, not a definition to inline.

use lens_domain::{WrapperFinding, args_pass_through_by};

use crate::cli::{ParsedFile, QuintError, parse_source};
use crate::defs::{def_sites, is_builtin};
use crate::ir::Expr;

/// Every top-level definition in `source` that only forwards.
pub fn find_wrappers(source: &str) -> Result<Vec<WrapperFinding>, QuintError> {
    Ok(wrappers_of(&*parse_source(source)?))
}

/// One reference from a definition to another user definition: an
/// operator application or a bare name the name table resolves to a
/// definition. Feeds the wrapper analyzer's reuse counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRef {
    pub caller: String,
    pub callee: String,
}

/// Every reference from a top-level definition in `source` to a user
/// definition, in source order.
pub fn extract_call_refs(source: &str) -> Result<Vec<CallRef>, QuintError> {
    Ok(call_refs_of(&*parse_source(source)?))
}

pub(crate) fn call_refs_of(file: &ParsedFile) -> Vec<CallRef> {
    let mut out = Vec::new();
    for site in def_sites(file) {
        site.def.expr.walk(&mut |expr| {
            let callee = match expr {
                Expr::App { opcode, .. } if !is_builtin(opcode) => opcode,
                Expr::Name { id, name }
                    if file
                        .resolved
                        .get(id)
                        .is_some_and(|target| file.def_module.contains_key(target)) =>
                {
                    name
                }
                _ => return,
            };
            out.push(CallRef {
                caller: site.name.clone(),
                callee: callee.clone(),
            });
        });
    }
    out
}

pub(crate) fn wrappers_of(file: &ParsedFile) -> Vec<WrapperFinding> {
    let mut out = Vec::new();
    for site in def_sites(file) {
        let callee = match site.body {
            Expr::App { opcode, args, .. } if !is_builtin(opcode) => {
                let params: Vec<String> = site.params.iter().map(|p| (*p).to_owned()).collect();
                let passes = args_pass_through_by(args, &params, |arg| match arg {
                    Expr::Name { name, .. } => Some(name.clone()),
                    _ => None,
                });
                passes.then(|| opcode.clone())
            }
            Expr::Name { id, name } if site.params.is_empty() => file
                .resolved
                .get(id)
                .is_some_and(|target| file.def_module.contains_key(target))
                .then(|| name.clone()),
            _ => None,
        };
        let Some(callee) = callee else { continue };
        out.push(WrapperFinding {
            start_line: site.start_line,
            end_line: site.end_line,
            callee,
            adapters: Vec::new(),
            statement_count: 1,
            reuse: None,
            param_count: Some(site.params.len()),
            may_satisfy_interfaces: Vec::new(),
            name: site.name,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{counter, helpers};

    fn found(file: &ParsedFile) -> Vec<(String, String)> {
        wrappers_of(file)
            .into_iter()
            .map(|w| (w.name, w.callee))
            .collect()
    }

    #[test]
    fn forwarding_definitions_are_wrappers() {
        let pairs = found(&counter().file);
        assert_eq!(
            pairs,
            vec![
                ("forward".to_owned(), "double".to_owned()),
                ("stepAlias".to_owned(), "step".to_owned()),
            ],
        );
    }

    #[test]
    fn a_call_that_reshapes_its_arguments_is_not_a_wrapper() {
        // `twice(n) = inc(inc(n))` and `bump = x' = inc(x)` do work.
        assert!(found(&helpers().file).is_empty());
    }

    #[test]
    fn call_refs_cover_applications_and_resolved_names() {
        let refs: Vec<(String, String)> = call_refs_of(&counter().file)
            .into_iter()
            .map(|r| (r.caller, r.callee))
            .collect();
        for expected in [
            ("forward", "double"),
            ("bump", "inc"),
            ("step", "bump"),
            ("stepAlias", "step"),
            ("bumpTest", "init"),
        ] {
            assert!(
                refs.contains(&(expected.0.to_owned(), expected.1.to_owned())),
                "{expected:?} missing from {refs:?}"
            );
        }
        // A state variable is not a definition.
        assert!(!refs.iter().any(|(_, callee)| callee == "x"));
    }

    #[test]
    fn wrapper_records_its_arity_and_lines() {
        let w = wrappers_of(&counter().file).remove(0);
        assert_eq!((w.start_line, w.end_line, w.param_count), (12, 12, Some(1)));
    }
}

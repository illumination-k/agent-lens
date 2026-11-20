//! Complexity metrics for Quint definitions.
//!
//! Quint is expression-only, so branching lives in a handful of
//! operators:
//!
//! * `if (c) a else b` (`ite`) — one branch; an `ite` in the else arm of
//!   another reads as `else if` and is charged flat, a bare `else` costs
//!   one cognitive point.
//! * `match` (`matchVariant`) — one branch per arm past the first, one
//!   nesting-weighted cognitive point for the construct.
//! * `any { ... }` (`actionAny`) — a nondeterministic choice between
//!   actions; scored like `match`, one path per alternative.
//! * `and` / `or` / `implies` — a flattened logical chain: one McCabe
//!   path per operator, one cognitive point per run of like operators.
//!
//! Lambdas (`x => ...`, the bodies of `match` arms, `exists`, `map`, …)
//! raise the nesting of what they contain. `all { ... }` is a
//! conjunction, not a branch. Quint forbids recursion, so none is
//! charged. Halstead: built-in opcodes, calls, lambdas and `let`
//! qualifiers are operators; names and literals are operands.

use lens_domain::{ComplexityCounters, FunctionComplexity, HalsteadAcc};

use crate::cli::{ParsedFile, QuintError, parse_source};
use crate::defs::{def_sites, is_builtin};
use crate::ir::Expr;

/// Failure of [`extract_complexity_units`]: the parse itself.
pub type ComplexityError = QuintError;

/// One [`FunctionComplexity`] per top-level definition in `source`.
pub fn extract_complexity_units(source: &str) -> Result<Vec<FunctionComplexity>, ComplexityError> {
    Ok(complexity_units_of(&*parse_source(source)?))
}

pub(crate) fn complexity_units_of(file: &ParsedFile) -> Vec<FunctionComplexity> {
    def_sites(file)
        .into_iter()
        .map(|site| {
            let mut visitor = Visitor::default();
            for param in &site.params {
                visitor.halstead.operand(param);
            }
            visitor.visit(site.body);
            FunctionComplexity {
                cyclomatic: visitor.counters.cyclomatic(),
                cognitive: visitor.counters.cognitive(),
                max_nesting: visitor.counters.max_nesting(),
                halstead: visitor.halstead.counts(),
                is_test: site.is_test(),
                start_line: site.start_line,
                end_line: site.end_line,
                name: site.name,
            }
        })
        .collect()
}

const LOGICAL: &[&str] = &["and", "or", "implies"];

#[derive(Default)]
struct Visitor {
    counters: ComplexityCounters,
    halstead: HalsteadAcc,
}

impl Visitor {
    fn visit(&mut self, expr: &Expr) {
        match expr {
            Expr::Name { name, .. } => self.halstead.operand(name),
            Expr::Bool { value, .. } => self.halstead.operand(&value.to_string()),
            Expr::Int { value, .. } => self.halstead.operand(&value.to_string()),
            Expr::Str { value, .. } => self.halstead.operand(value),
            Expr::Lambda { params, expr, .. } => {
                self.halstead.op("=>");
                for p in params {
                    self.halstead.operand(&p.name);
                }
                self.nested(expr);
            }
            Expr::Let { opdef, expr, .. } => {
                self.halstead.op(&opdef.qualifier);
                self.halstead.operand(&opdef.name);
                self.visit(&opdef.expr);
                self.visit(expr);
            }
            Expr::App { opcode, args, .. } => self.visit_app(opcode, args),
        }
    }

    fn visit_app(&mut self, opcode: &str, args: &[Expr]) {
        match opcode {
            "ite" => {
                self.counters.add_branch();
                self.visit_ite(args);
            }
            "matchVariant" => {
                self.halstead.op(opcode);
                let arms = args.len().saturating_sub(1) / 2;
                self.counters
                    .add_branches(u32::try_from(arms.saturating_sub(1)).unwrap_or(u32::MAX));
                self.visit_all(args);
            }
            "actionAny" => {
                self.halstead.op(opcode);
                self.counters
                    .add_branches(u32::try_from(args.len().saturating_sub(1)).unwrap_or(u32::MAX));
                self.counters.enter_nest();
                self.visit_all(args);
                self.counters.exit_nest();
            }
            op if LOGICAL.contains(&op) => {
                let mut ops = Vec::new();
                let mut operands = Vec::new();
                flatten_logical(opcode, args, &mut ops, &mut operands);
                for op in &ops {
                    self.halstead.op(op);
                }
                self.counters.add_logical_chain(&ops);
                for operand in operands {
                    self.visit(operand);
                }
            }
            op if is_builtin(op) => {
                self.halstead.op(op);
                self.visit_all(args);
            }
            callee => {
                self.halstead.op("()");
                self.halstead.operand(callee);
                self.visit_all(args);
            }
        }
    }

    /// `[cond, then, else]` of an `ite` whose branch is already charged.
    fn visit_ite(&mut self, args: &[Expr]) {
        self.halstead.op("ite");
        let [cond, then, otherwise] = args else {
            self.visit_all(args);
            return;
        };
        self.visit(cond);
        self.nested(then);
        match otherwise {
            Expr::App { opcode, args, .. } if opcode == "ite" => {
                self.counters.add_flat_branch();
                self.visit_ite(args);
            }
            other => {
                self.counters.add_cognitive(1);
                self.nested(other);
            }
        }
    }

    fn nested(&mut self, expr: &Expr) {
        self.counters.enter_nest();
        self.visit(expr);
        self.counters.exit_nest();
    }

    fn visit_all(&mut self, args: &[Expr]) {
        for arg in args {
            self.visit(arg);
        }
    }
}

/// Flatten directly nested `and` / `or` / `implies` into the operator
/// sequence in source order, collecting the non-logical operands.
fn flatten_logical<'a>(
    opcode: &'a str,
    args: &'a [Expr],
    ops: &mut Vec<&'a str>,
    operands: &mut Vec<&'a Expr>,
) {
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            ops.push(opcode);
        }
        match arg {
            Expr::App {
                opcode: inner,
                args: inner_args,
                ..
            } if LOGICAL.contains(&inner.as_str()) => {
                flatten_logical(inner, inner_args, ops, operands);
            }
            other => operands.push(other),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::counter;
    use rstest::rstest;

    fn unit(name: &str) -> FunctionComplexity {
        complexity_units_of(&counter().file)
            .into_iter()
            .find(|u| u.name == name)
            .unwrap()
    }

    #[rstest]
    // `if` + flat `else if` + bare `else`.
    #[case::else_if_chain("classify", 14, 17, 3, 3, 1)]
    // `and` then `or`: two operators, two runs.
    #[case::logical_chain("guard", 19, 19, 3, 2, 0)]
    // Two arms, one nested `if` inside an arm lambda.
    #[case::match_arms("handle", 21, 24, 3, 4, 2)]
    // `any` over three actions.
    #[case::nondeterministic_choice("step", 30, 34, 3, 1, 1)]
    #[case::straight_line("double", 10, 10, 1, 0, 0)]
    fn scores_quint_control_structures(
        #[case] name: &str,
        #[case] start: usize,
        #[case] end: usize,
        #[case] cyclomatic: u32,
        #[case] cognitive: u32,
        #[case] nesting: u32,
    ) {
        let u = unit(name);
        assert_eq!(
            (
                u.start_line,
                u.end_line,
                u.cyclomatic,
                u.cognitive,
                u.max_nesting
            ),
            (start, end, cyclomatic, cognitive, nesting),
        );
    }

    #[test]
    fn variant_constructors_are_not_units() {
        let names: Vec<String> = complexity_units_of(&counter().file)
            .into_iter()
            .map(|u| u.name)
            .collect();
        assert!(
            !names.iter().any(|n| n == "Inc" || n == "Reset"),
            "{names:?}"
        );
        assert_eq!(names.first().map(String::as_str), Some("double"));
    }

    #[test]
    fn run_definitions_are_tests() {
        assert!(unit("bumpTest").is_test);
        assert!(!unit("step").is_test);
    }

    #[test]
    fn halstead_counts_operators_and_operands() {
        // `n * 2`, with the parameter `n` counted once at the signature.
        let h = unit("double").halstead;
        assert_eq!((h.total_operators, h.total_operands), (1, 3));
    }
}

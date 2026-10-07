//! Inter-procedural taint tracking over per-function flow summaries.
//!
//! A language adapter describes each function as a [`FunctionFlow`]:
//! the untrusted values it introduces ([`TaintSource`]), every call it
//! makes with *where each argument may have come from* ([`Origin`]),
//! and where its return values may have come from. That description is
//! flow-insensitive and syntax-only — the adapter never needs to know
//! which calls reach the workspace and which leave it.
//!
//! [`trace_taint`] then joins those descriptions with the call graph's
//! resolution of each call site and runs a summary-based fixpoint:
//!
//! * a function's **return summary** says which of its parameters (and
//!   which sources) can flow into what it returns, so a caller's
//!   `id := parse(r)` is tainted exactly when `parse` passes `r` through;
//! * its **sink summary** says which parameters reach a dangerous call,
//!   directly or through callees, with the call-site path that gets
//!   them there.
//!
//! A call the graph could not resolve to a workspace function is
//! treated as library code: its result carries everything its receiver
//! and arguments carry, unless the adapter marked it a sanitizer. That
//! default is what lets `fmt.Sprintf("… %s", name)` or
//! `r.URL.Query().Get("id")` carry taint without a model of either.
//!
//! The result is a list of [`TaintFinding`]s — source, sink, and the
//! shortest call-site path found between them. Every set involved grows
//! monotonically over a finite domain, so the fixpoint terminates.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Where a value inside one function may have come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Origin {
    /// The function's own parameter at this position (receiver excluded).
    Param(usize),
    /// One of the function's [`FunctionFlow::sources`], by index.
    Source(usize),
    /// The result of one of the function's [`FunctionFlow::calls`], by
    /// index. Resolved by [`trace_taint`] against the callee's summary.
    CallResult(usize),
    /// Everything that flows *into* one of the function's calls (its
    /// receiver and arguments) when that call is library code, and
    /// nothing when it resolves to a workspace function or sanitizes.
    /// This is how an adapter says "the call writes its inputs into this
    /// variable": `json.NewDecoder(r.Body).Decode(&v)` taints `v`,
    /// `b.WriteString(s)` taints `b`. A workspace callee's body is
    /// summarised instead, and the summary does not track writes.
    CallInputs(usize),
}

/// An untrusted value a function introduces — a request parameter, say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintSource {
    /// What the value is, as the agent should read it
    /// (`r *http.Request`).
    pub label: String,
    pub line: usize,
}

/// A call whose listed arguments must not carry untrusted data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkSpec {
    /// Vulnerability class (`command-injection`, `sql-injection`, …).
    pub kind: &'static str,
    /// The call as the rule names it (`os/exec.Command`, `.Query`).
    pub rule: String,
    /// Argument positions checked at this call site.
    pub arguments: Vec<usize>,
}

/// Which arguments of a sink call are checked, independent of how many
/// a particular call site passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgSelector {
    All,
    At(usize),
    From(usize),
}

impl ArgSelector {
    /// The checked positions at a call site passing `argument_count`
    /// arguments.
    pub fn positions(self, argument_count: usize) -> Vec<usize> {
        match self {
            Self::All => (0..argument_count).collect(),
            Self::At(position) => (position < argument_count)
                .then_some(position)
                .into_iter()
                .collect(),
            Self::From(start) => (start..argument_count).collect(),
        }
    }
}

/// One call site inside a function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowCall {
    pub line: usize,
    /// The callee's bare name — the key the call graph's edges carry,
    /// used to join this site to its resolved target.
    pub callee_name: Option<String>,
    /// The callee as written (`exec.Command`, `s.runTool`), for reports.
    pub callee_label: String,
    /// Origins of the receiver expression of a method call.
    pub receiver: BTreeSet<Origin>,
    /// Written as `recv.method(..)`: the receiver is not among
    /// `arguments`. A path call to a method (`Self::helper(self, x)`,
    /// Go's `T.M(recv, x)`) passes the receiver as its first argument
    /// instead, which shifts every parameter one position right.
    pub method_syntax: bool,
    /// Origins of each argument, in source order.
    pub arguments: Vec<BTreeSet<Origin>>,
    /// The result is clean whatever goes in (`strconv.Atoi`, `len`).
    pub sanitizer: bool,
    /// Set when the call is a known dangerous API. Ignored when the call
    /// resolves to a workspace function: then its body is the truth.
    pub sink: Option<SinkSpec>,
}

/// What [`trace_taint`] needs to know about one function.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FunctionFlow {
    /// Line the declaration starts on: the key that joins the flow to
    /// its call-graph node. [`trace_taint`] does not read it.
    pub start_line: usize,
    /// Declared with a receiver (`self`, a Go method receiver) that is
    /// not one of its parameter slots.
    pub takes_receiver: bool,
    pub sources: Vec<TaintSource>,
    pub calls: Vec<FlowCall>,
    /// Origins of every value the function returns.
    pub returns: BTreeSet<Origin>,
}

/// A source, as a `(function, index into its sources)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SourceRef {
    pub function: usize,
    pub source: usize,
}

/// A call site, as a `(function, index into its calls)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CallRef {
    pub function: usize,
    pub call: usize,
}

/// One argument of one call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ArgRef {
    pub call: CallRef,
    pub argument: usize,
}

/// Untrusted data from `source` reaches `sink`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintFinding {
    pub source: SourceRef,
    /// The tainted argument of the sink call.
    pub sink: ArgRef,
    /// The tainted argument at each call site, from the function where
    /// the tainted value is first passed on down to the sink call itself
    /// (always the last entry, equal to `sink`). A source returned out of
    /// a helper starts the path at the helper's caller, not inside it.
    pub path: Vec<ArgRef>,
}

/// A value's origin once call results are resolved: a parameter of the
/// function being summarised, or a concrete source anywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Taint {
    Param(usize),
    Source(SourceRef),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Summary {
    returns: BTreeSet<Taint>,
    /// Parameter → sink it reaches → shortest path found.
    param_sinks: BTreeMap<(usize, ArgRef), Vec<ArgRef>>,
}

/// Run the fixpoint. `callees` is parallel to `flows`, and each entry
/// parallel to that function's `calls`: the workspace function each call
/// site resolves to, or `None` for library and unresolved calls.
///
/// Findings come back sorted by sink, then source.
pub fn trace_taint(flows: &[FunctionFlow], callees: &[Vec<Option<usize>>]) -> Vec<TaintFinding> {
    let callee_of = |function: usize, call: usize| {
        callees
            .get(function)
            .and_then(|calls| calls.get(call))
            .copied()
            .flatten()
            .filter(|&callee| callee < flows.len())
    };
    let mut callers: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); flows.len()];
    for (function, flow) in flows.iter().enumerate() {
        for call in 0..flow.calls.len() {
            if let Some(callee) = callee_of(function, call) {
                callers[callee].insert(function);
            }
        }
    }

    let mut summaries = vec![Summary::default(); flows.len()];
    let mut findings: BTreeMap<(ArgRef, SourceRef), Vec<ArgRef>> = BTreeMap::new();
    let mut queue: VecDeque<usize> = (0..flows.len()).collect();
    let mut queued = vec![true; flows.len()];
    while let Some(function) = queue.pop_front() {
        queued[function] = false;
        let summary = summarise(function, flows, &callee_of, &summaries, &mut findings);
        if summary != summaries[function] {
            summaries[function] = summary;
            for &caller in &callers[function] {
                if !queued[caller] {
                    queued[caller] = true;
                    queue.push_back(caller);
                }
            }
        }
    }

    findings
        .into_iter()
        .map(|((sink, source), path)| TaintFinding { source, sink, path })
        .collect()
}

/// Recompute one function's summary from the current summaries of its
/// callees, recording every source-to-sink flow it completes.
fn summarise(
    function: usize,
    flows: &[FunctionFlow],
    callee_of: &impl Fn(usize, usize) -> Option<usize>,
    summaries: &[Summary],
    findings: &mut BTreeMap<(ArgRef, SourceRef), Vec<ArgRef>>,
) -> Summary {
    let flow = &flows[function];
    let resolved: Vec<bool> = (0..flow.calls.len())
        .map(|call| callee_of(function, call).is_some())
        .collect();
    let results = resolve_call_results(function, flows, callee_of, summaries, &resolved);
    let abs = |origins: &BTreeSet<Origin>| abstract_origins(function, origins, &results, &resolved);

    let mut summary = Summary {
        returns: abs(&flow.returns),
        param_sinks: BTreeMap::new(),
    };
    for (index, call) in flow.calls.iter().enumerate() {
        let here = CallRef {
            function,
            call: index,
        };
        // `(argument position, sink, path beyond this call site)`.
        let reached: Vec<(usize, ArgRef, &[ArgRef])> = match callee_of(function, index) {
            Some(callee) => {
                let shift = receiver_shift(call, &flows[callee]);
                summaries[callee]
                    .param_sinks
                    .iter()
                    .map(|(&(param, sink), path)| (param + shift, sink, path.as_slice()))
                    .collect()
            }
            None => call.sink.as_ref().map_or_else(Vec::new, |spec| {
                spec.arguments
                    .iter()
                    .map(|&argument| {
                        (
                            argument,
                            ArgRef {
                                call: here,
                                argument,
                            },
                            &[][..],
                        )
                    })
                    .collect()
            }),
        };
        for (position, sink, rest) in reached {
            let Some(argument) = call.arguments.get(position) else {
                continue;
            };
            for taint in abs(argument) {
                let mut path = Vec::with_capacity(rest.len() + 1);
                path.push(ArgRef {
                    call: here,
                    argument: position,
                });
                path.extend_from_slice(rest);
                match taint {
                    Taint::Param(param) => {
                        keep_shortest(&mut summary.param_sinks, (param, sink), path)
                    }
                    Taint::Source(source) => keep_shortest(findings, (sink, source), path),
                }
            }
        }
    }
    summary
}

/// How many leading arguments of `call` are not parameter slots of
/// `callee`: one when a receiver-taking callee is called by path with the
/// receiver passed first, zero otherwise.
fn receiver_shift(call: &FlowCall, callee: &FunctionFlow) -> usize {
    usize::from(callee.takes_receiver && !call.method_syntax)
}

/// What each call's result carries, iterated to a fixpoint because a
/// flow-insensitive body can feed a call's result back into its own
/// arguments (`x = f(x)`).
fn resolve_call_results(
    function: usize,
    flows: &[FunctionFlow],
    callee_of: &impl Fn(usize, usize) -> Option<usize>,
    summaries: &[Summary],
    resolved: &[bool],
) -> Vec<BTreeSet<Taint>> {
    let flow = &flows[function];
    let mut results: Vec<BTreeSet<Taint>> = vec![BTreeSet::new(); flow.calls.len()];
    loop {
        let mut changed = false;
        for (index, call) in flow.calls.iter().enumerate() {
            if call.sanitizer {
                continue;
            }
            let abs = |origins: &BTreeSet<Origin>| {
                abstract_origins(function, origins, &results, resolved)
            };
            let next: BTreeSet<Taint> = match callee_of(function, index) {
                Some(callee) => {
                    let shift = receiver_shift(call, &flows[callee]);
                    summaries[callee]
                        .returns
                        .iter()
                        .flat_map(|taint| match *taint {
                            Taint::Param(param) => call
                                .arguments
                                .get(param + shift)
                                .map(&abs)
                                .unwrap_or_default(),
                            Taint::Source(source) => BTreeSet::from([Taint::Source(source)]),
                        })
                        .collect()
                }
                None => call
                    .arguments
                    .iter()
                    .chain(std::iter::once(&call.receiver))
                    .flat_map(&abs)
                    .collect(),
            };
            if !next.is_subset(&results[index]) {
                results[index].extend(next);
                changed = true;
            }
        }
        if !changed {
            return results;
        }
    }
}

/// Replace call-relative origins by what the calls currently carry. An
/// unresolved, non-sanitizing call's result *is* the union of its inputs,
/// so [`Origin::CallInputs`] reads the same slot — but only for calls
/// that did not resolve.
fn abstract_origins(
    function: usize,
    origins: &BTreeSet<Origin>,
    results: &[BTreeSet<Taint>],
    resolved: &[bool],
) -> BTreeSet<Taint> {
    let mut out = BTreeSet::new();
    for origin in origins {
        match *origin {
            Origin::Param(param) => {
                out.insert(Taint::Param(param));
            }
            Origin::Source(source) => {
                out.insert(Taint::Source(SourceRef { function, source }));
            }
            Origin::CallResult(call) => {
                if let Some(result) = results.get(call) {
                    out.extend(result.iter().copied());
                }
            }
            Origin::CallInputs(call) => {
                if !resolved.get(call).copied().unwrap_or(true)
                    && let Some(result) = results.get(call)
                {
                    out.extend(result.iter().copied());
                }
            }
        }
    }
    out
}

/// Insert `path` under `key` unless an equally short or shorter one is
/// already there. Shorter wins so a report leads with the most direct
/// route; ties keep the first found, which the deterministic worklist
/// order makes stable.
fn keep_shortest<K: Ord>(map: &mut BTreeMap<K, Vec<ArgRef>>, key: K, path: Vec<ArgRef>) {
    match map.get(&key) {
        Some(existing) if existing.len() <= path.len() => {}
        _ => {
            map.insert(key, path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    fn origins(items: &[Origin]) -> BTreeSet<Origin> {
        items.iter().copied().collect()
    }

    fn call(name: &str, arguments: Vec<BTreeSet<Origin>>) -> FlowCall {
        FlowCall {
            line: 1,
            callee_name: Some(name.to_owned()),
            callee_label: name.to_owned(),
            receiver: BTreeSet::new(),
            method_syntax: true,
            arguments,
            sanitizer: false,
            sink: None,
        }
    }

    fn sink(name: &str, arguments: Vec<BTreeSet<Origin>>) -> FlowCall {
        FlowCall {
            sink: Some(SinkSpec {
                kind: "command-injection",
                rule: name.to_owned(),
                arguments: (0..arguments.len()).collect(),
            }),
            ..call(name, arguments)
        }
    }

    fn source() -> Vec<TaintSource> {
        vec![TaintSource {
            label: "r *http.Request".to_owned(),
            line: 1,
        }]
    }

    fn path(steps: &[(usize, usize)]) -> Vec<CallRef> {
        steps
            .iter()
            .map(|&(function, call)| CallRef { function, call })
            .collect()
    }

    fn calls(finding: &TaintFinding) -> Vec<CallRef> {
        finding.path.iter().map(|step| step.call).collect()
    }

    #[test]
    fn a_source_passed_straight_to_a_sink_is_a_finding() {
        let flows = vec![FunctionFlow {
            sources: source(),
            calls: vec![sink("exec.Command", vec![origins(&[Origin::Source(0)])])],
            ..FunctionFlow::default()
        }];
        let findings = trace_taint(&flows, &[vec![None]]);
        assert_eq!(findings.len(), 1);
        assert_eq!(calls(&findings[0]), path(&[(0, 0)]));
    }

    #[test]
    fn a_parameter_carries_taint_through_a_chain_of_workspace_calls() {
        // handler(r) -> mid(x) -> leaf(y) -> exec.Command(y)
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![call("mid", vec![origins(&[Origin::Source(0)])])],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![call("leaf", vec![origins(&[Origin::Param(0)])])],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![sink("exec.Command", vec![origins(&[Origin::Param(0)])])],
                ..FunctionFlow::default()
            },
        ];
        let findings = trace_taint(&flows, &[vec![Some(1)], vec![Some(2)], vec![None]]);
        assert_eq!(findings.len(), 1);
        assert_eq!(calls(&findings[0]), path(&[(0, 0), (1, 0), (2, 0)]));
        assert_eq!(
            findings[0].sink.call,
            CallRef {
                function: 2,
                call: 0
            }
        );
    }

    #[test]
    fn only_the_parameter_that_reaches_the_sink_matters() {
        // leaf(a, b) sinks only b; the handler passes the source as a.
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![call(
                    "leaf",
                    vec![origins(&[Origin::Source(0)]), BTreeSet::new()],
                )],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![sink("exec.Command", vec![origins(&[Origin::Param(1)])])],
                ..FunctionFlow::default()
            },
        ];
        assert!(trace_taint(&flows, &[vec![Some(1)], vec![None]]).is_empty());
    }

    #[rstest]
    #[case::library_call_propagates(false, 1)]
    #[case::sanitizer_cleans(true, 0)]
    fn unresolved_calls_propagate_unless_they_sanitize(
        #[case] sanitizer: bool,
        #[case] expected: usize,
    ) {
        // q := fmt.Sprintf(.., src) / strconv.Atoi(src); exec.Command(q)
        let flows = vec![FunctionFlow {
            sources: source(),
            calls: vec![
                FlowCall {
                    sanitizer,
                    ..call("convert", vec![origins(&[Origin::Source(0)])])
                },
                sink("exec.Command", vec![origins(&[Origin::CallResult(0)])]),
            ],
            ..FunctionFlow::default()
        }];
        assert_eq!(trace_taint(&flows, &[vec![None, None]]).len(), expected);
    }

    #[rstest]
    #[case::passes_its_parameter_through(Origin::Param(0), 1)]
    #[case::returns_something_unrelated(Origin::Param(1), 0)]
    fn a_workspace_call_result_follows_the_callee_return_summary(
        #[case] returned: Origin,
        #[case] expected: usize,
    ) {
        // id := helper(src, "x"); exec.Command(id)
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![
                    call(
                        "helper",
                        vec![origins(&[Origin::Source(0)]), BTreeSet::new()],
                    ),
                    sink("exec.Command", vec![origins(&[Origin::CallResult(0)])]),
                ],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                returns: origins(&[returned]),
                ..FunctionFlow::default()
            },
        ];
        assert_eq!(
            trace_taint(&flows, &[vec![Some(1), None], vec![]]).len(),
            expected
        );
    }

    #[test]
    fn a_source_returned_from_a_helper_is_tracked_into_the_caller() {
        let flows = vec![
            FunctionFlow {
                calls: vec![
                    call("getName", vec![]),
                    sink("exec.Command", vec![origins(&[Origin::CallResult(0)])]),
                ],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                sources: source(),
                returns: origins(&[Origin::Source(0)]),
                ..FunctionFlow::default()
            },
        ];
        let findings = trace_taint(&flows, &[vec![Some(1), None], vec![]]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].source,
            SourceRef {
                function: 1,
                source: 0
            }
        );
        assert_eq!(calls(&findings[0]), path(&[(0, 1)]));
    }

    #[test]
    fn a_resolved_call_is_followed_even_when_it_looks_like_a_sink() {
        // A workspace method named like a sink is judged by its body.
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![sink("Query", vec![origins(&[Origin::Source(0)])])],
                ..FunctionFlow::default()
            },
            FunctionFlow::default(),
        ];
        assert!(trace_taint(&flows, &[vec![Some(1)], vec![]]).is_empty());
    }

    #[test]
    fn recursion_terminates_and_still_finds_the_sink() {
        // walk(x) calls itself and sinks x.
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![call("walk", vec![origins(&[Origin::Source(0)])])],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![
                    call("walk", vec![origins(&[Origin::Param(0)])]),
                    sink("exec.Command", vec![origins(&[Origin::Param(0)])]),
                ],
                returns: origins(&[Origin::CallResult(0)]),
                ..FunctionFlow::default()
            },
        ];
        let findings = trace_taint(&flows, &[vec![Some(1)], vec![Some(1), None]]);
        assert_eq!(findings.len(), 1);
        assert_eq!(calls(&findings[0]), path(&[(0, 0), (1, 1)]));
    }

    #[test]
    fn a_call_result_feeding_its_own_argument_reaches_a_fixpoint() {
        // x = f(x, src) in a flow-insensitive body.
        let flows = vec![FunctionFlow {
            sources: source(),
            calls: vec![
                call(
                    "f",
                    vec![
                        origins(&[Origin::CallResult(0)]),
                        origins(&[Origin::Source(0)]),
                    ],
                ),
                sink("exec.Command", vec![origins(&[Origin::CallResult(0)])]),
            ],
            ..FunctionFlow::default()
        }];
        assert_eq!(trace_taint(&flows, &[vec![None, None]]).len(), 1);
    }

    #[rstest]
    #[case::method_syntax_lines_up(true, Origin::Source(0), BTreeSet::new(), 1)]
    #[case::method_syntax_misses_the_wrong_slot(true, Origin::Param(9), origins(&[Origin::Source(0)]), 0)]
    #[case::path_call_passes_the_receiver_first(false, Origin::Param(9), origins(&[Origin::Source(0)]), 1)]
    fn a_path_call_to_a_method_shifts_parameters_past_the_receiver(
        #[case] method_syntax: bool,
        #[case] first: Origin,
        #[case] second: BTreeSet<Origin>,
        #[case] expected: usize,
    ) {
        // helper(&self, x) sinks x, and returns x too: the caller sinks
        // the result as well, so both the sink and the return summaries
        // must shift.
        let arguments = vec![origins(&[first]), second];
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![
                    FlowCall {
                        method_syntax,
                        ..call("helper", arguments)
                    },
                    sink("Command::new", vec![origins(&[Origin::CallResult(0)])]),
                ],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                takes_receiver: true,
                calls: vec![sink("Command::new", vec![origins(&[Origin::Param(0)])])],
                returns: origins(&[Origin::Param(0)]),
                ..FunctionFlow::default()
            },
        ];
        let findings = trace_taint(&flows, &[vec![Some(1), None], vec![None]]);
        assert_eq!(findings.len(), expected * 2, "{findings:?}");
    }

    #[rstest]
    #[case(ArgSelector::All, 2, vec![0, 1])]
    #[case(ArgSelector::At(1), 2, vec![1])]
    #[case(ArgSelector::At(2), 2, vec![])]
    #[case(ArgSelector::From(1), 3, vec![1, 2])]
    fn selected_positions(
        #[case] selector: ArgSelector,
        #[case] count: usize,
        #[case] expected: Vec<usize>,
    ) {
        assert_eq!(selector.positions(count), expected);
    }

    #[test]
    fn the_shortest_path_wins_whichever_is_found_last() {
        // handler -> leaf (sink) directly, and handler -> mid -> leaf.
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![
                    call("leaf", vec![origins(&[Origin::Source(0)])]),
                    call("mid", vec![origins(&[Origin::Source(0)])]),
                ],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![sink("exec.Command", vec![origins(&[Origin::Param(0)])])],
                ..FunctionFlow::default()
            },
            FunctionFlow {
                calls: vec![call("leaf", vec![origins(&[Origin::Param(0)])])],
                ..FunctionFlow::default()
            },
        ];
        let findings = trace_taint(&flows, &[vec![Some(1), Some(2)], vec![None], vec![Some(1)]]);
        assert_eq!(findings.len(), 1);
        assert_eq!(calls(&findings[0]), path(&[(0, 0), (1, 0)]));
    }

    #[rstest]
    #[case::library_call_writes_its_inputs(None, 1)]
    #[case::workspace_call_is_judged_by_its_summary(Some(1), 0)]
    fn call_inputs_flow_only_out_of_library_calls(
        #[case] callee: Option<usize>,
        #[case] expected: usize,
    ) {
        // decode(src, &v); exec.Command(v)
        let flows = vec![
            FunctionFlow {
                sources: source(),
                calls: vec![
                    call(
                        "decode",
                        vec![origins(&[Origin::Source(0)]), BTreeSet::new()],
                    ),
                    sink("exec.Command", vec![origins(&[Origin::CallInputs(0)])]),
                ],
                ..FunctionFlow::default()
            },
            FunctionFlow::default(),
        ];
        assert_eq!(
            trace_taint(&flows, &[vec![callee, None], vec![]]).len(),
            expected
        );
    }

    #[test]
    fn out_of_range_callee_indices_are_ignored() {
        let flows = vec![FunctionFlow {
            sources: source(),
            calls: vec![sink("exec.Command", vec![origins(&[Origin::Source(0)])])],
            ..FunctionFlow::default()
        }];
        // A bogus callee index falls back to the call's own sink spec;
        // one past the end is the boundary.
        assert_eq!(trace_taint(&flows, &[vec![Some(1)]]).len(), 1);
    }
}

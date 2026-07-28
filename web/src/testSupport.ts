/**
 * Fixture builders shared by the viewer test suites.
 *
 * `graph.test.ts` and `viewer/layout.test.ts` both need well-formed
 * `GraphNode` / `GraphEdge` values, and every field of both types is
 * required — so hand-writing them per suite is a large, drifting
 * copy-paste. These builders take only what a test actually cares about
 * and fill the rest with inert defaults.
 *
 * Not imported by any production module, so it never reaches the bundle.
 */
import type { FunctionGraphReport, GraphEdge, GraphNode } from "./graph";

export function makeNode(
  id: string,
  name: string,
  qualifiedName: string,
  isTest: boolean,
  weights: Partial<GraphNode["weights"]>,
  file = "src/lib.rs",
): GraphNode {
  return {
    id,
    name,
    qualified_name: qualifiedName,
    file,
    module: "crate",
    impl_owner: null,
    start_line: 1,
    end_line: 3,
    is_test: isTest,
    weights: {
      incoming_call_count: 0,
      outgoing_call_count: 0,
      fan_in: 0,
      fan_out: 0,
      loc: 3,
      cyclomatic_complexity: null,
      cognitive_complexity: null,
      max_nesting: null,
      maintainability_index: null,
      halstead_volume: null,
      total_time_ms: null,
      self_time_ms: null,
      error_count: null,
      ...weights,
    },
  };
}

export function makeEdge(
  from: string | null,
  to: string | null,
  resolution: GraphEdge["resolution"],
  callCount: number,
): GraphEdge {
  return {
    from,
    to,
    callee_name: to,
    resolution,
    call_count: callCount,
    call_lines: [1],
    weights: {
      call_count: callCount,
      total_transition_time_ms: null,
      error_count: null,
    },
  };
}

/**
 * A three-node report: `a` calls `b`, the test `c` also calls `b`, and
 * `a` has one unresolved call. Small enough to reason about, but it
 * covers the resolved / unresolved and test / non-test splits every
 * view filter keys off.
 */
export function makeReport(): FunctionGraphReport {
  return {
    schema_version: 1,
    root: "/repo",
    language: "rust",
    node_count: 3,
    edge_count: 3,
    nodes: [
      makeNode("a", "load", "crate::load", false, {
        fan_in: 2,
        fan_out: 1,
        incoming_call_count: 2,
        outgoing_call_count: 1,
      }),
      makeNode("b", "render", "crate::render", false, {
        fan_in: 1,
        fan_out: 0,
        incoming_call_count: 1,
        outgoing_call_count: 0,
      }),
      makeNode("c", "test_render", "crate::tests::test_render", true, {
        fan_in: 0,
        fan_out: 1,
        incoming_call_count: 0,
        outgoing_call_count: 1,
      }),
    ],
    edges: [
      makeEdge("a", "b", "resolved", 2),
      makeEdge("c", "b", "resolved", 1),
      makeEdge("a", null, "unresolved", 1),
    ],
    summary: {
      resolved_edge_count: 2,
      unresolved_edge_count: 1,
      ambiguous_edge_count: 0,
      anonymous_edge_count: 0,
      total_static_call_count: 4,
    },
  };
}

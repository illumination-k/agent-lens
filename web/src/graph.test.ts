import { describe, expect, it } from "vitest";
import {
  createGraphView,
  hiddenIncomingCallers,
  scoreNode,
  type FunctionGraphReport,
} from "./graph";

const report: FunctionGraphReport = {
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
    edge("a", "b", "resolved", 2),
    edge("c", "b", "resolved", 1),
    edge("a", null, "unresolved", 1),
  ],
  summary: {
    resolved_edge_count: 2,
    unresolved_edge_count: 1,
    ambiguous_edge_count: 0,
    anonymous_edge_count: 0,
    total_static_call_count: 4,
  },
};

describe("createGraphView", () => {
  it("filters test nodes and edges that reference them", () => {
    const view = createGraphView(report, { hideTests: true });

    expect(view.nodes.map((node) => node.id)).toEqual(["a", "b"]);
    expect(view.edges).toHaveLength(1);
    expect(view.unresolvedEdges).toHaveLength(1);
  });

  it("limits nodes by the selected score", () => {
    const view = createGraphView(report, { maxNodes: 1, metric: "fan_in" });

    expect(view.nodes.map((node) => node.id)).toEqual(["a"]);
    expect(view.stats.hiddenByLimit).toBe(2);
  });
});

describe("scoreNode", () => {
  it("uses total incoming and outgoing call counts for calls", () => {
    expect(scoreNode(report.nodes[0], "calls")).toBe(3);
  });
});

describe("hiddenIncomingCallers", () => {
  it("returns resolved incoming callers that are outside the current view", () => {
    const view = createGraphView(report, { maxNodes: 2, metric: "calls" });

    expect(view.nodes.map((node) => node.id)).toEqual(["a", "b"]);
    expect(hiddenIncomingCallers(report, report.nodes[1], view)).toEqual([
      {
        callCount: 1,
        node: report.nodes[2],
      },
    ]);
  });
});

function makeNode(
  id: string,
  name: string,
  qualifiedName: string,
  isTest: boolean,
  weights: Partial<FunctionGraphReport["nodes"][number]["weights"]>,
): FunctionGraphReport["nodes"][number] {
  return {
    id,
    name,
    qualified_name: qualifiedName,
    file: "src/lib.rs",
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

function edge(
  from: string | null,
  to: string | null,
  resolution: FunctionGraphReport["edges"][number]["resolution"],
  callCount: number,
): FunctionGraphReport["edges"][number] {
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

import { describe, expect, it } from "vitest";
import { createGraphView, hiddenIncomingCallers, scoreNode } from "./graph";
import { makeReport } from "./testSupport";

const report = makeReport();

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

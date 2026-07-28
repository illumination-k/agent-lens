import { describe, expect, it } from "vitest";
import { createGraphView } from "../graph";
import { makeNode, makeReport } from "../testSupport";
import {
  buildSimulation,
  centralityFor,
  clamp,
  GROUP_COLORS,
  groupColorFor,
  groupFor,
  hexToRgba,
  highComplexity,
  radiusFor,
  simulateGraph,
  stableHash,
} from "./layout";
import type { SimNode } from "./types";

describe("groupFor", () => {
  it("uses the crate name for a workspace path", () => {
    expect(groupFor(makeNode("a", "f", "f", false, {}, "crates/lens-rust/src/lib.rs"))).toBe(
      "lens-rust",
    );
  });

  it("uses the first path segment outside crates/", () => {
    expect(groupFor(makeNode("a", "f", "f", false, {}, "web/src/main.ts"))).toBe("web");
  });

  it("falls back to the whole path when there is no separator", () => {
    expect(groupFor(makeNode("a", "f", "f", false, {}, "build.rs"))).toBe("build.rs");
  });

  it("does not treat a bare crates/ path as a workspace member", () => {
    expect(groupFor(makeNode("a", "f", "f", false, {}, "crates"))).toBe("crates");
  });
});

describe("stableHash", () => {
  it("is deterministic for the same input", () => {
    expect(stableHash("lens-rust")).toBe(stableHash("lens-rust"));
  });

  it("separates different inputs", () => {
    expect(stableHash("lens-rust")).not.toBe(stableHash("lens-ts"));
  });

  it("stays an unsigned 32-bit integer", () => {
    for (const value of ["", "a", "crates/agent-lens/src/analyze/runner.rs", "\u{1f600}"]) {
      const hash = stableHash(value);
      expect(Number.isInteger(hash)).toBe(true);
      expect(hash).toBeGreaterThanOrEqual(0);
      expect(hash).toBeLessThan(2 ** 32);
    }
  });
});

describe("groupColorFor", () => {
  it("reuses the colour the layout already assigned to the group", () => {
    const groups = [{ key: "web", color: "#123456", x: 0, y: 0, size: 4 }];
    expect(groupColorFor("web", groups)).toBe("#123456");
  });

  it("falls back to a stable palette entry for an unknown group", () => {
    const expected = GROUP_COLORS[stableHash("absent") % GROUP_COLORS.length];
    expect(groupColorFor("absent", [])).toBe(expected);
    expect(groupColorFor("absent", [])).toBe(groupColorFor("absent", []));
  });
});

describe("centralityFor", () => {
  it("weights fan-in double", () => {
    const node = makeNode("a", "f", "f", false, {
      incoming_call_count: 1,
      outgoing_call_count: 2,
      fan_in: 3,
      fan_out: 4,
    });
    expect(centralityFor(node)).toBe(1 + 2 + 3 * 2 + 4);
  });
});

describe("highComplexity", () => {
  it("prefers cognitive complexity over cyclomatic", () => {
    const node = makeNode("a", "f", "f", false, {
      cognitive_complexity: 2,
      cyclomatic_complexity: 40,
    });
    expect(highComplexity(node)).toBe(false);
  });

  it("falls back to cyclomatic when cognitive is missing", () => {
    const node = makeNode("a", "f", "f", false, { cyclomatic_complexity: 18 });
    expect(highComplexity(node)).toBe(true);
  });

  it("treats a node with no complexity weights as simple", () => {
    expect(highComplexity(makeNode("a", "f", "f", false, {}))).toBe(false);
  });

  it("is inclusive at the threshold", () => {
    expect(highComplexity(makeNode("a", "f", "f", false, { cognitive_complexity: 17 }))).toBe(
      false,
    );
    expect(highComplexity(makeNode("a", "f", "f", false, { cognitive_complexity: 18 }))).toBe(true);
  });
});

describe("radiusFor", () => {
  it("grows with the selected metric", () => {
    const small = makeNode("a", "f", "f", false, { fan_in: 1 });
    const large = makeNode("b", "g", "g", false, { fan_in: 25 });
    expect(radiusFor(large, "fan_in")).toBeGreaterThan(radiusFor(small, "fan_in"));
  });

  it("keeps a positive floor for a zero score", () => {
    expect(radiusFor(makeNode("a", "f", "f", false, { fan_in: 0 }), "fan_in")).toBeGreaterThan(0);
  });
});

describe("hexToRgba", () => {
  it("splits the channels and appends the alpha", () => {
    expect(hexToRgba("#2563eb", 0.5)).toBe("rgba(37, 99, 235, 0.5)");
    expect(hexToRgba("#000000", 1)).toBe("rgba(0, 0, 0, 1)");
    expect(hexToRgba("#ffffff", 0)).toBe("rgba(255, 255, 255, 0)");
  });
});

describe("clamp", () => {
  it("bounds the value on both sides and passes it through in range", () => {
    expect(clamp(-5, 0, 10)).toBe(0);
    expect(clamp(15, 0, 10)).toBe(10);
    expect(clamp(4, 0, 10)).toBe(4);
  });
});

describe("buildSimulation", () => {
  const view = createGraphView(makeReport(), {});

  it("produces one sim node per view node with finite coordinates", () => {
    const state = buildSimulation([], view, "calls");

    expect(state.nodes.map((simNode) => simNode.node.id)).toEqual(["a", "b", "c"]);
    for (const simNode of state.nodes) {
      expect(Number.isFinite(simNode.x)).toBe(true);
      expect(Number.isFinite(simNode.y)).toBe(true);
      expect(simNode.vx).toBe(0);
      expect(simNode.vy).toBe(0);
      expect(simNode.radius).toBeGreaterThan(0);
    }
  });

  it("drops edges whose endpoints are not both in the view", () => {
    const state = buildSimulation([], view, "calls");

    // The report's third edge is unresolved (`to === null`), so only
    // the two resolved calls survive into the simulation.
    expect(state.edges).toHaveLength(2);
    for (const edge of state.edges) {
      expect(state.nodes).toContain(edge.source);
      expect(state.nodes).toContain(edge.target);
    }
  });

  it("reuses previous sim nodes so positions survive a rebuild", () => {
    const first = buildSimulation([], view, "calls");
    const moved = first.nodes[0] as SimNode;
    moved.x = 1234;
    moved.y = -99;

    const second = buildSimulation(first.nodes, view, "calls");

    expect(second.nodes[0]).toBe(moved);
    expect(second.nodes[0]?.x).toBe(1234);
    expect(second.nodes[0]?.y).toBe(-99);
  });

  it("groups nodes and assigns one colour per group", () => {
    const state = buildSimulation([], view, "calls");

    expect(state.layoutGroups.map((group) => group.key)).toEqual(["src"]);
    expect(state.layoutGroups[0]?.size).toBe(3);
    for (const simNode of state.nodes) {
      expect(simNode.group).toBe("src");
      expect(simNode.groupColor).toBe(state.layoutGroups[0]?.color);
    }
  });

  it("is deterministic for the same input", () => {
    const a = buildSimulation([], view, "calls");
    const b = buildSimulation([], view, "calls");

    expect(b.nodes.map((simNode) => [simNode.x, simNode.y])).toEqual(
      a.nodes.map((simNode) => [simNode.x, simNode.y]),
    );
    expect(b.labelCutoff).toBe(a.labelCutoff);
  });

  it("returns an infinite label cutoff when there are too few nodes to rank", () => {
    const empty = buildSimulation([], { ...view, edges: [], nodes: [] }, "calls");

    expect(empty.nodes).toEqual([]);
    expect(empty.edges).toEqual([]);
    expect(empty.layoutGroups).toEqual([]);
    expect(empty.labelCutoff).toBe(Infinity);
  });
});

describe("simulateGraph", () => {
  it("pushes two co-located nodes apart", () => {
    const state = buildSimulation([], createGraphView(makeReport(), {}), "calls");
    const [a, b] = state.nodes as [SimNode, SimNode];
    a.x = 0;
    a.y = 0;
    b.x = 0;
    b.y = 0;

    simulateGraph(state.nodes, []);

    expect(Math.abs(a.x - b.x) + Math.abs(a.y - b.y)).toBeGreaterThan(0);
  });

  it("keeps every coordinate finite over repeated steps", () => {
    const state = buildSimulation([], createGraphView(makeReport(), {}), "calls");

    for (let step = 0; step < 50; step += 1) {
      simulateGraph(state.nodes, state.edges);
    }

    for (const simNode of state.nodes) {
      expect(Number.isFinite(simNode.x)).toBe(true);
      expect(Number.isFinite(simNode.y)).toBe(true);
      expect(Number.isFinite(simNode.vx)).toBe(true);
      expect(Number.isFinite(simNode.vy)).toBe(true);
    }
  });

  it("does nothing to an empty graph", () => {
    expect(() => {
      simulateGraph([], []);
    }).not.toThrow();
  });
});

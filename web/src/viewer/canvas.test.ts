import { describe, expect, it } from "vitest";
import { makeEdge, makeNode } from "../testSupport";
import {
  colorFor,
  curvePoints,
  edgeColor,
  ghostPoint,
  isSelectedNeighbor,
  shouldLabelNode,
  strokeFor,
} from "./canvas";
import type { SimEdge, SimNode } from "./types";

function simNode(
  id: string,
  x: number,
  y: number,
  overrides: Partial<Pick<SimNode, "radius" | "centrality">> = {},
  isTest = false,
  weights: Parameters<typeof makeNode>[4] = {},
): SimNode {
  return {
    node: makeNode(id, id, id, isTest, weights),
    group: "web",
    groupColor: "#336699",
    centrality: overrides.centrality ?? 0,
    anchorX: x,
    anchorY: y,
    x,
    y,
    vx: 0,
    vy: 0,
    radius: overrides.radius ?? 6,
  };
}

function simEdge(source: SimNode, target: SimNode, curvature = 0): SimEdge {
  return {
    edge: makeEdge(source.node.id, target.node.id, "resolved", 1),
    source,
    target,
    curvature,
  };
}

describe("curvePoints", () => {
  it("offsets the endpoints by the node radii along the edge direction", () => {
    const source = simNode("a", 0, 0, { radius: 10 });
    const target = simNode("b", 100, 0, { radius: 20 });
    const curve = curvePoints(simEdge(source, target));
    expect(curve.startX).toBeCloseTo(12); // radius + 2
    expect(curve.startY).toBeCloseTo(0);
    expect(curve.endX).toBeCloseTo(100 - 24); // radius + 4
    expect(curve.endY).toBeCloseTo(0);
  });

  it("bends the control point away from a straight line by the curvature", () => {
    const source = simNode("a", 0, 0, { radius: 10 });
    const target = simNode("b", 100, 0, { radius: 10 });
    const straight = curvePoints(simEdge(source, target, 0));
    expect(straight.controlY).toBeCloseTo(0);

    const bent = curvePoints(simEdge(source, target, 0.1));
    expect(bent.controlX).toBeCloseTo(straight.controlX);
    // The bend is centre distance × curvature along the +y normal of a +x edge.
    expect(bent.controlY).toBeCloseTo(10);
  });
});

describe("ghostPoint", () => {
  it("starts straight above the selected node", () => {
    const selected = simNode("a", 50, 80);
    const point = ghostPoint(selected, 0, 8, 100);
    expect(point.x).toBeCloseTo(50);
    expect(point.y).toBeCloseTo(80 - 100);
  });

  it("spreads consecutive callers to distinct positions", () => {
    const selected = simNode("a", 0, 0);
    const first = ghostPoint(selected, 0, 8, 100);
    const second = ghostPoint(selected, 1, 8, 100);
    expect(Math.hypot(first.x - second.x, first.y - second.y)).toBeGreaterThan(10);
  });
});

describe("edgeColor", () => {
  it("assigns a distinct colour per resolution and threads the alpha through", () => {
    const colors = (["resolved", "unresolved", "ambiguous", "anonymous"] as const).map(
      (resolution) => edgeColor(resolution, 0.5),
    );
    expect(new Set(colors).size).toBe(4);
    for (const color of colors) {
      expect(color).toContain("0.5)");
    }
  });
});

describe("colorFor / strokeFor", () => {
  it("highlights the selected node", () => {
    const node = simNode("a", 0, 0);
    expect(colorFor(node, true, false)).toBe("#f7c948");
    expect(strokeFor(node, true)).toBe("#111827");
  });

  it("dims nodes outside the selected neighbourhood", () => {
    const node = simNode("a", 0, 0);
    expect(colorFor(node, false, true)).toContain("0.34");
    expect(colorFor(node, false, false)).toContain("0.86");
  });

  it("renders test nodes fainter and outlines high-complexity nodes", () => {
    const test = simNode("t", 0, 0, {}, true);
    expect(colorFor(test, false, false)).toContain("0.58");

    const complex = simNode("c", 0, 0, {}, false, { cognitive_complexity: 20 });
    expect(strokeFor(complex, false)).toBe("#b91c1c");
  });
});

describe("isSelectedNeighbor", () => {
  const a = simNode("a", 0, 0);
  const b = simNode("b", 10, 0);
  const c = simNode("c", 20, 0);
  const edges = [simEdge(a, b)];

  it("treats every node as a neighbour when nothing is selected", () => {
    expect(isSelectedNeighbor(edges, null, c)).toBe(true);
  });

  it("includes the selected node and nodes connected in either direction", () => {
    expect(isSelectedNeighbor(edges, "a", a)).toBe(true);
    expect(isSelectedNeighbor(edges, "a", b)).toBe(true);
    expect(isSelectedNeighbor(edges, "b", a)).toBe(true);
  });

  it("excludes unconnected nodes", () => {
    expect(isSelectedNeighbor(edges, "a", c)).toBe(false);
  });
});

describe("shouldLabelNode", () => {
  const camera = { x: 0, y: 0, zoom: 1 };

  it("labels the selected node and its neighbours", () => {
    const a = simNode("a", 0, 0);
    const b = simNode("b", 10, 0);
    const state = { camera, edges: [simEdge(a, b)], labelCutoff: Infinity, selectedId: "a" };
    expect(shouldLabelNode(state, a)).toBe(true);
    expect(shouldLabelNode(state, b)).toBe(true);
  });

  it("labels nodes at or above the centrality cutoff", () => {
    const state = { camera, edges: [], labelCutoff: 5, selectedId: null };
    expect(shouldLabelNode(state, simNode("a", 0, 0, { centrality: 5 }))).toBe(true);
    expect(shouldLabelNode(state, simNode("b", 0, 0, { centrality: 4 }))).toBe(false);
  });

  it("labels large nodes only once zoomed in", () => {
    const big = simNode("a", 0, 0, { radius: 8 });
    expect(
      shouldLabelNode({ camera, edges: [], labelCutoff: Infinity, selectedId: null }, big),
    ).toBe(false);
    expect(
      shouldLabelNode(
        { camera: { ...camera, zoom: 1.2 }, edges: [], labelCutoff: Infinity, selectedId: null },
        big,
      ),
    ).toBe(true);
  });
});

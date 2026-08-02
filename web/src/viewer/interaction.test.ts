import { describe, expect, it } from "vitest";
import { makeNode } from "../testSupport";
import {
  applyWheelZoom,
  computeFitCamera,
  FIT_MAX_ZOOM,
  MAX_ZOOM,
  MIN_ZOOM,
  nearestNode,
  screenToWorld,
} from "./interaction";
import type { SimNode } from "./types";

function simNode(id: string, x: number, y: number, radius: number): SimNode {
  return {
    node: makeNode(id, id, id, false, {}),
    group: "web",
    groupColor: "#123456",
    centrality: 0,
    anchorX: x,
    anchorY: y,
    x,
    y,
    vx: 0,
    vy: 0,
    radius,
  };
}

describe("computeFitCamera", () => {
  it("resets to the identity camera for an empty graph", () => {
    expect(computeFitCamera([], 800, 600)).toEqual({ x: 0, y: 0, zoom: 1 });
  });

  it("centres the camera on the node bounds", () => {
    const camera = computeFitCamera([simNode("a", 100, 40, 10)], 800, 600);
    // A single node is centred by translating its position (scaled by
    // zoom) back to the origin.
    expect(camera.x).toBeCloseTo(-100 * camera.zoom);
    expect(camera.y).toBeCloseTo(-40 * camera.zoom);
  });

  it("clamps zoom to the fit range", () => {
    const tiny = computeFitCamera([simNode("a", 0, 0, 1)], 4000, 4000);
    expect(tiny.zoom).toBeLessThanOrEqual(FIT_MAX_ZOOM);

    const sprawling = computeFitCamera(
      [simNode("a", -100_000, 0, 10), simNode("b", 100_000, 0, 10)],
      800,
      600,
    );
    expect(sprawling.zoom).toBe(MIN_ZOOM);
  });
});

describe("applyWheelZoom", () => {
  it("zooms out for a positive wheel delta and in for a negative one", () => {
    expect(applyWheelZoom(1, 120)).toBeCloseTo(0.9);
    expect(applyWheelZoom(1, -120)).toBeCloseTo(1.1);
  });

  it("clamps to the interactive zoom range", () => {
    expect(applyWheelZoom(MIN_ZOOM, 120)).toBe(MIN_ZOOM);
    expect(applyWheelZoom(MAX_ZOOM, -120)).toBe(MAX_ZOOM);
  });
});

describe("nearestNode", () => {
  it("returns null when nothing is within reach", () => {
    expect(nearestNode([simNode("a", 0, 0, 5)], 100, 100)).toBeNull();
  });

  it("hits a node within its radius plus the hit slop", () => {
    const node = simNode("a", 0, 0, 5);
    expect(nearestNode([node], 10, 0)).toBe(node);
    expect(nearestNode([node], 12, 0)).toBeNull();
  });

  it("prefers the node whose centre is closest", () => {
    const near = simNode("near", 0, 0, 20);
    const far = simNode("far", 10, 0, 20);
    expect(nearestNode([far, near], 2, 0)).toBe(near);
  });
});

describe("screenToWorld", () => {
  it("maps the canvas centre to the world origin for an identity camera", () => {
    const point = screenToWorld({ x: 0, y: 0, zoom: 1 }, 800, 600, 1, 400, 300);
    expect(point).toEqual({ x: 0, y: 0 });
  });

  it("accounts for pixel ratio, camera offset, and zoom", () => {
    const camera = { x: 50, y: -30, zoom: 2 };
    const point = screenToWorld(camera, 800, 600, 2, 100, 200);
    expect(point.x).toBeCloseTo((100 * 2 - 400 - 50) / 2);
    expect(point.y).toBeCloseTo((200 * 2 - 300 + 30) / 2);
  });
});

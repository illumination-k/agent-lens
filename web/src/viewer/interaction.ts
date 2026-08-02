import { clamp } from "./layout";
import type { Camera, SimNode } from "./types";

export const MIN_ZOOM = 0.25;
export const MAX_ZOOM = 3.5;
export const FIT_MAX_ZOOM = 2.5;

/** Camera that frames every node with some margin, at unit zoom for an empty graph. */
export function computeFitCamera(nodes: SimNode[], width: number, height: number): Camera {
  if (nodes.length === 0) {
    return { x: 0, y: 0, zoom: 1 };
  }
  const bounds = nodes.reduce(
    (acc, node) => ({
      minX: Math.min(acc.minX, node.x - node.radius),
      maxX: Math.max(acc.maxX, node.x + node.radius),
      minY: Math.min(acc.minY, node.y - node.radius),
      maxY: Math.max(acc.maxY, node.y + node.radius),
    }),
    { minX: Infinity, maxX: -Infinity, minY: Infinity, maxY: -Infinity },
  );
  const boundsWidth = Math.max(1, bounds.maxX - bounds.minX);
  const boundsHeight = Math.max(1, bounds.maxY - bounds.minY);
  const zoom = clamp(
    Math.min((width * 0.82) / boundsWidth, (height * 0.82) / boundsHeight, FIT_MAX_ZOOM),
    MIN_ZOOM,
    FIT_MAX_ZOOM,
  );
  return {
    zoom,
    x: -((bounds.minX + bounds.maxX) / 2) * zoom,
    y: -((bounds.minY + bounds.maxY) / 2) * zoom,
  };
}

/** Zoom in or out one wheel notch, clamped to the interactive zoom range. */
export function applyWheelZoom(zoom: number, deltaY: number): number {
  const delta = deltaY > 0 ? 0.9 : 1.1;
  return clamp(zoom * delta, MIN_ZOOM, MAX_ZOOM);
}

/** The node whose disc (plus a small hit slop) contains the point, nearest-center first. */
export function nearestNode(nodes: SimNode[], x: number, y: number): SimNode | null {
  let best: SimNode | null = null;
  let bestDistance = Infinity;
  for (const node of nodes) {
    const dx = node.x - x;
    const dy = node.y - y;
    const distance = Math.sqrt(dx * dx + dy * dy);
    if (distance <= node.radius + 6 && distance < bestDistance) {
      best = node;
      bestDistance = distance;
    }
  }
  return best;
}

/** Map CSS-pixel canvas coordinates to world coordinates under the camera. */
export function screenToWorld(
  camera: Camera,
  canvasWidth: number,
  canvasHeight: number,
  pixelRatio: number,
  x: number,
  y: number,
): { x: number; y: number } {
  return {
    x: (x * pixelRatio - canvasWidth / 2 - camera.x) / camera.zoom,
    y: (y * pixelRatio - canvasHeight / 2 - camera.y) / camera.zoom,
  };
}

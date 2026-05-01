import {
  scoreNode,
  type GraphEdge,
  type GraphNode,
  type GraphView,
  type NodeMetric,
} from "../graph";
import type { LayoutGroup, SimEdge, SimNode } from "./types";

export const GROUP_COLORS = [
  "#2563eb",
  "#0f766e",
  "#b45309",
  "#7c3aed",
  "#be123c",
  "#047857",
  "#0369a1",
  "#a16207",
  "#6d28d9",
  "#c2410c",
  "#0e7490",
  "#4d7c0f",
] as const;

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));

export type SimulationState = {
  edges: SimEdge[];
  labelCutoff: number;
  layoutGroups: LayoutGroup[];
  nodes: SimNode[];
};

export function buildSimulation(
  previousNodes: SimNode[],
  nextView: GraphView,
  metric: NodeMetric,
): SimulationState {
  const previous = new Map(previousNodes.map((node) => [node.node.id, node]));
  const groupKeys = sortedGroups(nextView.nodes);
  const colorByGroup = new Map(
    groupKeys.map((group, index) => [group, GROUP_COLORS[index % GROUP_COLORS.length]]),
  );
  const centerByGroup = new Map(groupCenters(groupKeys).map((group) => [group.key, group]));
  const groupOrdinal = new Map<string, number>();
  const count = Math.max(1, nextView.nodes.length);
  const layoutGroups = groupKeys.map((group) => {
    const center = centerByGroup.get(group);
    return {
      key: group,
      color: colorByGroup.get(group) ?? "#64748b",
      x: center?.x ?? 0,
      y: center?.y ?? 0,
      size: nextView.nodes.filter((node) => groupFor(node) === group).length,
    };
  });
  const nodes = nextView.nodes.map((node, index) => {
    const group = groupFor(node);
    const ordinal = groupOrdinal.get(group) ?? 0;
    groupOrdinal.set(group, ordinal + 1);
    const groupSize = layoutGroups.find((layoutGroup) => layoutGroup.key === group)?.size ?? count;
    const color = colorByGroup.get(group) ?? "#64748b";
    const center = centerByGroup.get(group) ?? { x: 0, y: 0 };
    const anchor = anchorFor(node, ordinal, groupSize, center.x, center.y);
    const old = previous.get(node.id);
    if (old !== undefined) {
      old.node = node;
      old.group = group;
      old.groupColor = color;
      old.centrality = centralityFor(node);
      old.anchorX = anchor.x;
      old.anchorY = anchor.y;
      old.radius = radiusFor(node, metric);
      return old;
    }
    return {
      node,
      group,
      groupColor: color,
      centrality: centralityFor(node),
      anchorX: anchor.x,
      anchorY: anchor.y,
      x: anchor.x + seededOffset(node.id, index, 22),
      y: anchor.y + seededOffset(`${node.id}:y`, index, 22),
      vx: 0,
      vy: 0,
      radius: radiusFor(node, metric),
    };
  });
  const labelCount = Math.max(8, Math.floor(nodes.length * 0.09));
  const labelCutoff =
    [...nodes].sort((a, b) => b.centrality - a.centrality)[labelCount - 1]?.centrality ?? Infinity;
  const byId = new Map(nodes.map((node) => [node.node.id, node]));
  const edges = nextView.edges.flatMap((edge) => {
    if (edge.from === null || edge.to === null) {
      return [];
    }
    const source = byId.get(edge.from);
    const target = byId.get(edge.to);
    return source !== undefined && target !== undefined
      ? [{ edge, source, target, curvature: curvatureFor(source, target, edge) }]
      : [];
  });
  return { edges, labelCutoff, layoutGroups, nodes };
}

export function simulateGraph(nodes: SimNode[], edges: SimEdge[]): void {
  const repulsion = 7200;
  for (let i = 0; i < nodes.length; i += 1) {
    for (let j = i + 1; j < nodes.length; j += 1) {
      const a = nodes[i];
      const b = nodes[j];
      if (a === undefined || b === undefined) {
        continue;
      }
      const dx = a.x - b.x;
      const dy = a.y - b.y;
      const minDistance = a.radius + b.radius + (a.group === b.group ? 14 : 28);
      const distanceSq = Math.max(minDistance * minDistance, dx * dx + dy * dy);
      const groupFactor = a.group === b.group ? 0.55 : 1.1;
      const force = (repulsion * groupFactor) / distanceSq;
      const distance = Math.sqrt(distanceSq);
      const fx = (dx / distance) * force;
      const fy = (dy / distance) * force;
      a.vx += fx;
      a.vy += fy;
      b.vx -= fx;
      b.vy -= fy;
    }
  }
  for (const edge of edges) {
    const dx = edge.target.x - edge.source.x;
    const dy = edge.target.y - edge.source.y;
    const distance = Math.max(1, Math.sqrt(dx * dx + dy * dy));
    const desired =
      (edge.source.group === edge.target.group ? 74 : 150) +
      edge.source.radius +
      edge.target.radius;
    const force = (distance - desired) * 0.006 * Math.min(4.5, Math.log2(edge.edge.call_count + 1));
    const fx = (dx / distance) * force;
    const fy = (dy / distance) * force;
    edge.source.vx += fx;
    edge.source.vy += fy;
    edge.target.vx -= fx;
    edge.target.vy -= fy;
  }
  for (const node of nodes) {
    const anchorStrength = node.node.is_test ? 0.0038 : 0.0024;
    node.vx += (node.anchorX - node.x) * anchorStrength;
    node.vy += (node.anchorY - node.y) * anchorStrength;
    node.vx += -node.x * 0.00055;
    node.vy += -node.y * 0.00055;
    node.vx *= 0.84;
    node.vy *= 0.84;
    node.x += node.vx;
    node.y += node.vy;
  }
}

export function radiusFor(node: GraphNode, metric: NodeMetric): number {
  return 5 + Math.sqrt(Math.max(1, scoreNode(node, metric))) * 2.2;
}

export function highComplexity(node: GraphNode): boolean {
  return (node.weights.cognitive_complexity ?? node.weights.cyclomatic_complexity ?? 0) >= 18;
}

export function groupFor(node: GraphNode): string {
  const parts = node.file.split("/");
  if (parts[0] === "crates" && parts[1] !== undefined) {
    return parts[1];
  }
  return parts[0] ?? "root";
}

export function groupColorFor(group: string, layoutGroups: LayoutGroup[]): string {
  const existing = layoutGroups.find((layoutGroup) => layoutGroup.key === group)?.color;
  if (existing !== undefined) {
    return existing;
  }
  return GROUP_COLORS[stableHash(group) % GROUP_COLORS.length] ?? "#64748b";
}

export function centralityFor(node: GraphNode): number {
  return (
    node.weights.incoming_call_count +
    node.weights.outgoing_call_count +
    node.weights.fan_in * 2 +
    node.weights.fan_out
  );
}

export function stableHash(value: string): number {
  let hash = 2166136261;
  for (let index = 0; index < value.length; index += 1) {
    hash ^= value.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return hash >>> 0;
}

export function hexToRgba(hex: string, alpha: number): string {
  const value = Number.parseInt(hex.slice(1), 16);
  const red = (value >> 16) & 255;
  const green = (value >> 8) & 255;
  const blue = value & 255;
  return `rgba(${red}, ${green}, ${blue}, ${alpha})`;
}

export function clamp(value: number, min: number, max: number): number {
  return Math.min(max, Math.max(min, value));
}

function sortedGroups(graphNodes: GraphNode[]): string[] {
  const counts = new Map<string, number>();
  for (const node of graphNodes) {
    const group = groupFor(node);
    counts.set(group, (counts.get(group) ?? 0) + 1);
  }
  return [...counts.entries()]
    .sort((a, b) => b[1] - a[1] || a[0].localeCompare(b[0]))
    .map(([group]) => group);
}

function groupCenters(groups: string[]): LayoutGroup[] {
  if (groups.length === 0) {
    return [];
  }
  const ring = 180 + Math.sqrt(groups.length) * 72;
  return groups.map((group, index) => {
    const angle = (Math.PI * 2 * index) / groups.length - Math.PI / 2;
    return {
      key: group,
      color: GROUP_COLORS[index % GROUP_COLORS.length] ?? "#64748b",
      x: Math.cos(angle) * ring,
      y: Math.sin(angle) * ring,
      size: 0,
    };
  });
}

function anchorFor(
  node: GraphNode,
  ordinal: number,
  groupSize: number,
  centerX: number,
  centerY: number,
): { x: number; y: number } {
  const angle = ordinal * GOLDEN_ANGLE;
  const rankRadius = 18 + Math.sqrt(ordinal + 1) * 15;
  const maxRadius = 54 + Math.sqrt(groupSize) * 22;
  const roleOffset = clamp((node.weights.fan_out - node.weights.fan_in) * 7, -68, 68);
  const testOffset = node.is_test ? 52 : 0;
  const radius = Math.min(maxRadius, rankRadius + testOffset);
  return {
    x: centerX + Math.cos(angle) * radius + roleOffset,
    y: centerY + Math.sin(angle) * radius + clamp((node.weights.loc - 30) * 0.75, -42, 42),
  };
}

function curvatureFor(source: SimNode, target: SimNode, edge: GraphEdge): number {
  const base = source.group === target.group ? 0.055 : 0.14;
  const sign =
    stableHash(`${edge.from ?? ""}:${edge.to ?? ""}:${edge.callee_name ?? ""}`) % 2 === 0 ? 1 : -1;
  return base * sign;
}

function seededOffset(seed: string, index: number, spread: number): number {
  const fraction = (stableHash(`${seed}:${index}`) % 1000) / 1000 - 0.5;
  return fraction * spread;
}

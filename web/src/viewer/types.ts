import type { GraphEdge, GraphNode } from "../graph";

export type SimNode = {
  node: GraphNode;
  group: string;
  groupColor: string;
  centrality: number;
  anchorX: number;
  anchorY: number;
  x: number;
  y: number;
  vx: number;
  vy: number;
  radius: number;
};

export type SimEdge = {
  edge: GraphEdge;
  source: SimNode;
  target: SimNode;
  curvature: number;
};

export type LayoutGroup = {
  key: string;
  color: string;
  x: number;
  y: number;
  size: number;
};

export type Camera = {
  x: number;
  y: number;
  zoom: number;
};

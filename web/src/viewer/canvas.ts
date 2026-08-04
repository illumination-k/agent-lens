import type { HiddenCaller, Resolution } from "../graph";
import { groupColorFor, groupFor, hexToRgba, highComplexity } from "./layout";
import type { Camera, LayoutGroup, SimEdge, SimNode } from "./types";

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const MAX_GHOST_CALLERS = 48;

/** Everything the renderer needs to paint one frame. */
export type RenderState = {
  camera: Camera;
  edges: SimEdge[];
  /** Hidden incoming callers of the selected node, or empty when nothing is selected. */
  hiddenCallers: HiddenCaller[];
  labelCutoff: number;
  layoutGroups: LayoutGroup[];
  nodes: SimNode[];
  selectedId: string | null;
};

export function draw(context: CanvasRenderingContext2D, state: RenderState): void {
  const { canvas } = context;
  const { camera } = state;
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save();
  context.translate(canvas.width / 2 + camera.x, canvas.height / 2 + camera.y);
  context.scale(camera.zoom, camera.zoom);

  drawGroups(context, state);

  for (const edge of state.edges) {
    drawEdge(context, state, edge);
  }

  drawHiddenIncomingCallers(context, state);

  for (const node of state.nodes) {
    const selected = node.node.id === state.selectedId;
    const dimmed =
      state.selectedId !== null && !isSelectedNeighbor(state.edges, state.selectedId, node);
    context.beginPath();
    context.fillStyle = colorFor(node, selected, dimmed);
    context.strokeStyle = strokeFor(node, selected);
    context.lineWidth = (selected ? 3 : highComplexity(node.node) ? 2.2 : 1) / camera.zoom;
    context.arc(node.x, node.y, node.radius, 0, Math.PI * 2);
    context.fill();
    context.stroke();
  }

  if (camera.zoom > 0.55) {
    context.font = `${12 / camera.zoom}px Inter, system-ui, sans-serif`;
    context.fillStyle = "#17202a";
    context.textAlign = "center";
    for (const node of state.nodes) {
      if (!shouldLabelNode(state, node)) {
        continue;
      }
      context.fillText(node.node.name, node.x, node.y - node.radius - 6 / camera.zoom);
    }
  }
  context.restore();
}

export function drawEmpty(context: CanvasRenderingContext2D): void {
  const { canvas } = context;
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.fillStyle = "#f6f7f9";
  context.fillRect(0, 0, canvas.width, canvas.height);
}

function drawGroups(context: CanvasRenderingContext2D, state: RenderState): void {
  const { camera } = state;
  if (camera.zoom < 0.18) {
    return;
  }
  context.save();
  for (const group of state.layoutGroups) {
    const members = state.nodes.filter((node) => node.group === group.key);
    if (members.length === 0) {
      continue;
    }
    const radius =
      Math.max(
        54,
        ...members.map((node) => Math.hypot(node.x - group.x, node.y - group.y) + node.radius + 28),
      ) /
      camera.zoom ** 0.08;
    context.beginPath();
    context.fillStyle = hexToRgba(group.color, 0.055);
    context.strokeStyle = hexToRgba(group.color, 0.18);
    context.lineWidth = 1 / camera.zoom;
    context.arc(group.x, group.y, radius, 0, Math.PI * 2);
    context.fill();
    context.stroke();

    if (camera.zoom > 0.32) {
      context.fillStyle = hexToRgba(group.color, 0.76);
      context.font = `${11 / camera.zoom}px Inter, system-ui, sans-serif`;
      context.textAlign = "center";
      context.fillText(`${group.key} ${group.size}`, group.x, group.y - radius - 8 / camera.zoom);
    }
  }
  context.restore();
}

function drawEdge(context: CanvasRenderingContext2D, state: RenderState, edge: SimEdge): void {
  const { camera, selectedId } = state;
  const selected = edge.source.node.id === selectedId || edge.target.node.id === selectedId;
  const highlighted = selectedId !== null && !selected;
  const curve = curvePoints(edge);
  const callWeight = Math.min(3.5, 0.55 + Math.log2(edge.edge.call_count + 1) * 0.45);
  const alpha = selected
    ? 0.82
    : highlighted
      ? 0.08
      : 0.2 + Math.min(0.18, edge.edge.call_count * 0.018);

  context.save();
  context.strokeStyle = edgeColor(edge.edge.resolution, alpha);
  context.lineWidth = (selected ? callWeight + 1.1 : callWeight) / camera.zoom;
  if (edge.edge.resolution !== "resolved") {
    context.setLineDash([8 / camera.zoom, 5 / camera.zoom]);
  }
  context.beginPath();
  context.moveTo(curve.startX, curve.startY);
  context.quadraticCurveTo(curve.controlX, curve.controlY, curve.endX, curve.endY);
  context.stroke();
  context.setLineDash([]);

  if (selected || edge.edge.call_count > 1 || camera.zoom > 0.8) {
    drawArrowhead(
      context,
      camera.zoom,
      curve.controlX,
      curve.controlY,
      curve.endX,
      curve.endY,
      edgeColor(edge.edge.resolution, alpha + 0.14),
    );
  }
  context.restore();
}

export function curvePoints(edge: SimEdge): {
  startX: number;
  startY: number;
  controlX: number;
  controlY: number;
  endX: number;
  endY: number;
} {
  const dx = edge.target.x - edge.source.x;
  const dy = edge.target.y - edge.source.y;
  const distance = Math.max(1, Math.hypot(dx, dy));
  const unitX = dx / distance;
  const unitY = dy / distance;
  const normalX = -unitY;
  const normalY = unitX;
  const startX = edge.source.x + unitX * (edge.source.radius + 2);
  const startY = edge.source.y + unitY * (edge.source.radius + 2);
  const endX = edge.target.x - unitX * (edge.target.radius + 4);
  const endY = edge.target.y - unitY * (edge.target.radius + 4);
  const bend = distance * edge.curvature;
  return {
    startX,
    startY,
    controlX: (startX + endX) / 2 + normalX * bend,
    controlY: (startY + endY) / 2 + normalY * bend,
    endX,
    endY,
  };
}

function drawArrowhead(
  context: CanvasRenderingContext2D,
  zoom: number,
  fromX: number,
  fromY: number,
  toX: number,
  toY: number,
  color: string,
): void {
  const angle = Math.atan2(toY - fromY, toX - fromX);
  const size = 8 / zoom;
  context.save();
  context.fillStyle = color;
  context.beginPath();
  context.moveTo(toX, toY);
  context.lineTo(toX - Math.cos(angle - 0.48) * size, toY - Math.sin(angle - 0.48) * size);
  context.lineTo(toX - Math.cos(angle + 0.48) * size, toY - Math.sin(angle + 0.48) * size);
  context.closePath();
  context.fill();
  context.restore();
}

function drawHiddenIncomingCallers(context: CanvasRenderingContext2D, state: RenderState): void {
  const { camera, hiddenCallers, layoutGroups, selectedId } = state;
  if (selectedId === null || hiddenCallers.length === 0) {
    return;
  }
  const selected = state.nodes.find((node) => node.node.id === selectedId);
  if (selected === undefined) {
    return;
  }
  const visible = hiddenCallers.slice(0, MAX_GHOST_CALLERS);
  const ring = selected.radius + 76 + Math.sqrt(visible.length) * 15;
  context.save();
  context.setLineDash([4 / camera.zoom, 5 / camera.zoom]);
  visible.forEach((caller, index) => {
    const point = ghostPoint(selected, index, visible.length, ring);
    const color = groupColorFor(groupFor(caller.node), layoutGroups);
    const alpha = 0.2 + Math.min(0.18, caller.callCount * 0.025);
    context.strokeStyle = hexToRgba(color, alpha);
    context.lineWidth = (0.9 + Math.min(2.2, Math.log2(caller.callCount + 1) * 0.45)) / camera.zoom;
    context.beginPath();
    context.moveTo(point.x, point.y);
    context.lineTo(selected.x, selected.y);
    context.stroke();
    drawArrowhead(
      context,
      camera.zoom,
      point.x,
      point.y,
      selected.x,
      selected.y,
      hexToRgba(color, 0.34),
    );
  });
  context.setLineDash([]);
  visible.forEach((caller, index) => {
    const point = ghostPoint(selected, index, visible.length, ring);
    const color = groupColorFor(groupFor(caller.node), layoutGroups);
    const radius = 4.5 + Math.min(5.5, Math.sqrt(caller.callCount) * 1.2);
    context.beginPath();
    context.fillStyle = hexToRgba(color, 0.22);
    context.strokeStyle = hexToRgba(color, 0.48);
    context.lineWidth = 1 / camera.zoom;
    context.arc(point.x, point.y, radius, 0, Math.PI * 2);
    context.fill();
    context.stroke();
    if (camera.zoom > 0.85 && (index < 12 || caller.callCount > 1)) {
      context.fillStyle = "rgba(71, 85, 105, 0.82)";
      context.font = `${10 / camera.zoom}px Inter, system-ui, sans-serif`;
      context.textAlign = "center";
      context.fillText(caller.node.name, point.x, point.y - radius - 5 / camera.zoom);
    }
  });
  if (hiddenCallers.length > visible.length) {
    const point = ghostPoint(selected, visible.length, visible.length + 1, ring);
    context.beginPath();
    context.fillStyle = "rgba(71, 85, 105, 0.2)";
    context.strokeStyle = "rgba(71, 85, 105, 0.5)";
    context.lineWidth = 1 / camera.zoom;
    context.arc(point.x, point.y, 8 / camera.zoom ** 0.15, 0, Math.PI * 2);
    context.fill();
    context.stroke();
    context.fillStyle = "rgba(51, 65, 85, 0.9)";
    context.font = `${11 / camera.zoom}px Inter, system-ui, sans-serif`;
    context.textAlign = "center";
    context.fillText(
      `+${hiddenCallers.length - visible.length}`,
      point.x,
      point.y + 4 / camera.zoom,
    );
  }
  context.restore();
}

export function ghostPoint(
  selected: SimNode,
  index: number,
  count: number,
  ring: number,
): { x: number; y: number } {
  const angle = index * GOLDEN_ANGLE - Math.PI / 2;
  const radius = ring + (index % 5) * 11 + Math.floor(index / Math.max(1, count / 3)) * 16;
  return {
    x: selected.x + Math.cos(angle) * radius,
    y: selected.y + Math.sin(angle) * radius,
  };
}

export function colorFor(node: SimNode, selected: boolean, dimmed: boolean): string {
  if (selected) {
    return "#f7c948";
  }
  if (dimmed) {
    return hexToRgba(node.groupColor, 0.34);
  }
  return node.node.is_test ? hexToRgba(node.groupColor, 0.58) : hexToRgba(node.groupColor, 0.86);
}

export function strokeFor(node: SimNode, selected: boolean): string {
  if (selected) {
    return "#111827";
  }
  if (highComplexity(node.node)) {
    return "#b91c1c";
  }
  if (node.node.is_test) {
    return "rgba(71, 85, 105, 0.64)";
  }
  return "rgba(15, 23, 42, 0.42)";
}

export function edgeColor(resolution: Resolution, alpha: number): string {
  switch (resolution) {
    case "ambiguous":
      return `rgba(180, 83, 9, ${alpha})`;
    case "anonymous":
      return `rgba(109, 40, 217, ${alpha})`;
    case "unresolved":
      return `rgba(190, 18, 60, ${alpha})`;
    case "resolved":
      return `rgba(51, 65, 85, ${alpha})`;
  }
}

export function shouldLabelNode(
  state: Pick<RenderState, "camera" | "edges" | "labelCutoff" | "selectedId">,
  node: SimNode,
): boolean {
  return (
    node.node.id === state.selectedId ||
    (state.selectedId !== null && isSelectedNeighbor(state.edges, state.selectedId, node)) ||
    node.centrality >= state.labelCutoff ||
    (state.camera.zoom > 1.15 && node.radius >= 8)
  );
}

export function isSelectedNeighbor(
  edges: SimEdge[],
  selectedId: string | null,
  node: SimNode,
): boolean {
  if (selectedId === null) {
    return true;
  }
  if (node.node.id === selectedId) {
    return true;
  }
  return edges.some(
    (edge) =>
      (edge.source.node.id === selectedId && edge.target.node.id === node.node.id) ||
      (edge.target.node.id === selectedId && edge.source.node.id === node.node.id),
  );
}

import {
  createGraphView,
  edgeRows,
  hiddenIncomingCallers,
  scoreNode,
  topNodes,
  type FunctionGraphReport,
  type GraphEdge,
  type GraphOptions,
  type GraphView,
  type HiddenCaller,
  type NodeMetric,
  type Resolution,
} from "./graph";
import "./styles.css";
import { escapeAttr, escapeHtml, formatNumber, getCanvasContext, getElement } from "./viewer/dom";
import {
  buildSimulation,
  clamp,
  groupColorFor,
  groupFor,
  hexToRgba,
  highComplexity,
  simulateGraph,
} from "./viewer/layout";
import type { LayoutGroup, SimEdge, SimNode } from "./viewer/types";

const GOLDEN_ANGLE = Math.PI * (3 - Math.sqrt(5));
const MAX_GHOST_CALLERS = 48;

const app = document.querySelector<HTMLDivElement>("#app");
if (app === null) {
  throw new Error("Missing #app root");
}

const canvas = getElement<HTMLCanvasElement>("graph");
const context = getCanvasContext(canvas);

const controls = {
  query: getElement<HTMLInputElement>("query"),
  metric: getElement<HTMLSelectElement>("metric"),
  resolution: getElement<HTMLSelectElement>("resolution"),
  maxNodes: getElement<HTMLInputElement>("max-nodes"),
  maxNodesValue: getElement<HTMLOutputElement>("max-nodes-value"),
  minCalls: getElement<HTMLInputElement>("min-calls"),
  minCallsValue: getElement<HTMLOutputElement>("min-calls-value"),
  hideTests: getElement<HTMLInputElement>("hide-tests"),
  upload: getElement<HTMLInputElement>("upload"),
  fit: getElement<HTMLButtonElement>("fit"),
  restart: getElement<HTMLButtonElement>("restart"),
};

const panels = {
  loadState: getElement<HTMLElement>("load-state"),
  stats: getElement<HTMLElement>("stats"),
  ranking: getElement<HTMLElement>("ranking"),
  details: getElement<HTMLElement>("details"),
};

let report: FunctionGraphReport | null = null;
let view: GraphView | null = null;
let nodes: SimNode[] = [];
let edges: SimEdge[] = [];
let layoutGroups: LayoutGroup[] = [];
let labelCutoff = Infinity;
let selectedId: string | null = null;
let animationFrame: number | null = null;
let settleTicks = 0;
let camera = { x: 0, y: 0, zoom: 1 };
let dragState:
  | { mode: "node"; node: SimNode }
  | { mode: "pan"; x: number; y: number; cameraX: number; cameraY: number }
  | null = null;

wireControls();
resizeCanvas();
window.addEventListener("resize", () => {
  resizeCanvas();
  fitView();
  draw();
});

void loadInitialReport();

async function loadInitialReport(): Promise<void> {
  const embedded = readEmbeddedReport();
  if (embedded !== null) {
    await setReport(embedded, "ssg");
    return;
  }
  try {
    const response = await fetch(`${import.meta.env.BASE_URL}function-graph.json`, {
      cache: "no-store",
    });
    if (!response.ok) {
      throw new Error(`HTTP ${response.status}`);
    }
    await setReport((await response.json()) as FunctionGraphReport, "ready");
  } catch {
    panels.loadState.textContent = "no data";
    panels.details.innerHTML = `
      <h2>No graph loaded</h2>
      <p>Generate <code>public/function-graph.json</code> or load an analyzer JSON file.</p>
    `;
    drawEmpty();
  }
}

function readEmbeddedReport(): FunctionGraphReport | null {
  const element = document.getElementById("function-graph-data");
  const payload = element?.textContent?.trim();
  if (payload === undefined || payload.length === 0) {
    return null;
  }
  return JSON.parse(payload) as FunctionGraphReport;
}

async function setReport(nextReport: FunctionGraphReport, state: string): Promise<void> {
  report = nextReport;
  panels.loadState.textContent = state;
  selectedId = null;
  refresh();
  fitView();
}

function wireControls(): void {
  for (const element of [
    controls.query,
    controls.metric,
    controls.resolution,
    controls.maxNodes,
    controls.minCalls,
    controls.hideTests,
  ]) {
    element.addEventListener("input", refresh);
  }
  controls.fit.addEventListener("click", () => {
    fitView();
    draw();
  });
  controls.restart.addEventListener("click", () => startSimulation(220));
  controls.upload.addEventListener("change", async () => {
    const file = controls.upload.files?.[0];
    if (file === undefined) {
      return;
    }
    await setReport(JSON.parse(await file.text()) as FunctionGraphReport, "loaded");
  });
  canvas.addEventListener("mousedown", onPointerDown);
  canvas.addEventListener("mousemove", onPointerMove);
  canvas.addEventListener("mouseup", () => {
    dragState = null;
  });
  canvas.addEventListener("mouseleave", () => {
    dragState = null;
  });
  canvas.addEventListener("wheel", onWheel, { passive: false });
}

function refresh(): void {
  if (report === null) {
    return;
  }
  const options = readOptions();
  controls.maxNodesValue.value = String(options.maxNodes);
  controls.minCallsValue.value = String(options.minCalls);
  view = createGraphView(report, options);
  rebuildSimulation(view, options.metric);
  renderPanels(report, view, options.metric);
  startSimulation(160);
}

function readOptions(): GraphOptions {
  return {
    query: controls.query.value,
    hideTests: controls.hideTests.checked,
    maxNodes: Number.parseInt(controls.maxNodes.value, 10),
    minCalls: Number.parseInt(controls.minCalls.value, 10),
    resolution: controls.resolution.value as Resolution | "all",
    metric: controls.metric.value as NodeMetric,
  };
}

function rebuildSimulation(nextView: GraphView, metric: NodeMetric): void {
  const simulation = buildSimulation(nodes, nextView, metric);
  nodes = simulation.nodes;
  edges = simulation.edges;
  layoutGroups = simulation.layoutGroups;
  labelCutoff = simulation.labelCutoff;
}

function startSimulation(ticks: number): void {
  settleTicks = ticks;
  if (animationFrame === null) {
    animationFrame = requestAnimationFrame(step);
  }
}

function step(): void {
  animationFrame = null;
  if (settleTicks > 0) {
    simulate();
    settleTicks -= 1;
    animationFrame = requestAnimationFrame(step);
  }
  draw();
}

function simulate(): void {
  simulateGraph(nodes, edges);
}

function draw(): void {
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.save();
  context.translate(canvas.width / 2 + camera.x, canvas.height / 2 + camera.y);
  context.scale(camera.zoom, camera.zoom);

  drawGroups();

  for (const edge of edges) {
    drawEdge(edge);
  }

  drawHiddenIncomingCallers();

  for (const node of nodes) {
    const selected = node.node.id === selectedId;
    context.beginPath();
    context.fillStyle = colorFor(node, selected);
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
    for (const node of nodes) {
      if (!shouldLabelNode(node)) {
        continue;
      }
      context.fillText(node.node.name, node.x, node.y - node.radius - 6 / camera.zoom);
    }
  }
  context.restore();
}

function drawEmpty(): void {
  context.clearRect(0, 0, canvas.width, canvas.height);
  context.fillStyle = "#f6f7f9";
  context.fillRect(0, 0, canvas.width, canvas.height);
}

function drawGroups(): void {
  if (camera.zoom < 0.18) {
    return;
  }
  context.save();
  for (const group of layoutGroups) {
    const members = nodes.filter((node) => node.group === group.key);
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

function drawEdge(edge: SimEdge): void {
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
      curve.controlX,
      curve.controlY,
      curve.endX,
      curve.endY,
      edgeColor(edge.edge.resolution, alpha + 0.14),
    );
  }
  context.restore();
}

function curvePoints(edge: SimEdge): {
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
  fromX: number,
  fromY: number,
  toX: number,
  toY: number,
  color: string,
): void {
  const angle = Math.atan2(toY - fromY, toX - fromX);
  const size = 8 / camera.zoom;
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

function drawHiddenIncomingCallers(): void {
  if (selectedId === null || view === null) {
    return;
  }
  const selected = nodes.find((node) => node.node.id === selectedId);
  const selectedViewNode = view.nodes.find((node) => node.id === selectedId);
  if (selected === undefined || selectedViewNode === undefined) {
    return;
  }
  if (report === null) {
    return;
  }
  const hiddenCallers = hiddenIncomingCallers(report, selectedViewNode, view);
  if (hiddenCallers.length === 0) {
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
    drawArrowhead(point.x, point.y, selected.x, selected.y, hexToRgba(color, 0.34));
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

function ghostPoint(
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

function renderPanels(
  currentReport: FunctionGraphReport,
  currentView: GraphView,
  metric: NodeMetric,
): void {
  panels.stats.innerHTML = `
    <dl>
      <div><dt>Visible</dt><dd>${currentView.stats.visibleNodes} / ${currentView.stats.totalNodes}</dd></div>
      <div><dt>Edges</dt><dd>${currentView.stats.visibleEdges} / ${currentView.stats.totalEdges}</dd></div>
      <div><dt>Modules</dt><dd>${new Set(currentView.nodes.map(groupFor)).size}</dd></div>
      <div><dt>Hidden</dt><dd>${currentView.stats.hiddenByLimit}</dd></div>
      <div><dt>Unresolved</dt><dd>${currentView.stats.unresolvedVisible}</dd></div>
    </dl>
  `;
  panels.ranking.innerHTML = `
    <h2>Top functions</h2>
    <ol>
      ${topNodes(currentReport, metric, 8)
        .map(
          (node) => `
            <li>
              <button type="button" data-node-id="${escapeAttr(node.id)}">
                <span>${escapeHtml(node.name)}</span>
                <strong>${scoreNode(node, metric).toFixed(0)}</strong>
              </button>
            </li>
          `,
        )
        .join("")}
    </ol>
  `;
  for (const button of panels.ranking.querySelectorAll<HTMLButtonElement>("button[data-node-id]")) {
    button.addEventListener("click", () => selectNode(button.dataset.nodeId ?? null));
  }
  renderDetails(currentView);
}

function renderDetails(currentView: GraphView): void {
  const selected =
    selectedId === null ? null : (currentView.nodes.find((node) => node.id === selectedId) ?? null);
  if (selected === null) {
    panels.details.innerHTML = `
      <h2>Inspect</h2>
      <p>Select a node to inspect calls, complexity, and source location.</p>
    `;
    return;
  }
  const outgoing = currentView.edges.filter((edge) => edge.from === selected.id);
  const incoming = currentView.edges.filter((edge) => edge.to === selected.id);
  const unresolved = currentView.unresolvedEdges.filter((edge) => edge.from === selected.id);
  const hiddenIncoming =
    report === null ? [] : hiddenIncomingCallers(report, selected, currentView);
  panels.details.innerHTML = `
    <h2>${escapeHtml(selected.name)}</h2>
    <p class="qualified">${escapeHtml(selected.qualified_name)}</p>
    <dl class="detail-grid">
      <div><dt>File</dt><dd>${escapeHtml(selected.file)}:${selected.start_line}</dd></div>
      <div><dt>Fan</dt><dd>${selected.weights.fan_in} in / ${selected.weights.fan_out} out</dd></div>
      <div><dt>Shown callers</dt><dd>${incoming.length} visible / ${hiddenIncoming.length} hidden</dd></div>
      <div><dt>Calls</dt><dd>${selected.weights.incoming_call_count} in / ${selected.weights.outgoing_call_count} out</dd></div>
      <div><dt>Complexity</dt><dd>${selected.weights.cognitive_complexity ?? "n/a"}</dd></div>
      <div><dt>LOC</dt><dd>${selected.weights.loc}</dd></div>
      <div><dt>MI</dt><dd>${formatNumber(selected.weights.maintainability_index)}</dd></div>
    </dl>
    <div class="columns">
      ${edgeList("Outgoing", outgoing, "to")}
      ${edgeList("Incoming", incoming, "from")}
      ${edgeList("Unresolved", unresolved, "callee_name")}
      ${hiddenCallerList(hiddenIncoming)}
    </div>
  `;
}

function edgeList(title: string, list: GraphEdge[], field: "from" | "to" | "callee_name"): string {
  const rows = edgeRows(list, field, view?.nodes ?? []).map(
    (edge) => `<li><span>${escapeHtml(edge.label)}</span><strong>${edge.count}</strong></li>`,
  );
  return `
    <section>
      <h3>${title}</h3>
      <ul>${rows.length > 0 ? rows.join("") : "<li><span>none</span><strong>0</strong></li>"}</ul>
    </section>
  `;
}

function hiddenCallerList(callers: HiddenCaller[]): string {
  const rows = callers
    .slice(0, 8)
    .map(
      (caller) =>
        `<li><span>${escapeHtml(caller.node.name)}</span><strong>${caller.callCount}</strong></li>`,
    );
  return `
    <section>
      <h3>Hidden callers</h3>
      <ul>${rows.length > 0 ? rows.join("") : "<li><span>none</span><strong>0</strong></li>"}</ul>
    </section>
  `;
}

function onPointerDown(event: MouseEvent): void {
  const point = screenToWorld(event.offsetX, event.offsetY);
  const hit = nearestNode(point.x, point.y);
  if (hit !== null) {
    selectNode(hit.node.id);
    dragState = { mode: "node", node: hit };
  } else {
    dragState = {
      mode: "pan",
      x: event.clientX,
      y: event.clientY,
      cameraX: camera.x,
      cameraY: camera.y,
    };
  }
}

function onPointerMove(event: MouseEvent): void {
  if (dragState === null) {
    return;
  }
  if (dragState.mode === "node") {
    const point = screenToWorld(event.offsetX, event.offsetY);
    dragState.node.x = point.x;
    dragState.node.y = point.y;
    dragState.node.vx = 0;
    dragState.node.vy = 0;
    startSimulation(30);
    return;
  }
  camera.x = dragState.cameraX + event.clientX - dragState.x;
  camera.y = dragState.cameraY + event.clientY - dragState.y;
  draw();
}

function onWheel(event: WheelEvent): void {
  event.preventDefault();
  const delta = event.deltaY > 0 ? 0.9 : 1.1;
  camera.zoom = clamp(camera.zoom * delta, 0.25, 3.5);
  draw();
}

function selectNode(id: string | null): void {
  selectedId = id;
  if (view !== null) {
    renderDetails(view);
  }
  draw();
}

function fitView(): void {
  if (nodes.length === 0) {
    camera = { x: 0, y: 0, zoom: 1 };
    return;
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
  const width = Math.max(1, bounds.maxX - bounds.minX);
  const height = Math.max(1, bounds.maxY - bounds.minY);
  const zoom = Math.min((canvas.width * 0.82) / width, (canvas.height * 0.82) / height, 2.5);
  camera = {
    zoom: clamp(zoom, 0.25, 2.5),
    x: -((bounds.minX + bounds.maxX) / 2) * clamp(zoom, 0.25, 2.5),
    y: -((bounds.minY + bounds.maxY) / 2) * clamp(zoom, 0.25, 2.5),
  };
}

function nearestNode(x: number, y: number): SimNode | null {
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

function screenToWorld(x: number, y: number): { x: number; y: number } {
  return {
    x: (x * devicePixelRatio - canvas.width / 2 - camera.x) / camera.zoom,
    y: (y * devicePixelRatio - canvas.height / 2 - camera.y) / camera.zoom,
  };
}

function resizeCanvas(): void {
  const rect = canvas.getBoundingClientRect();
  canvas.width = Math.max(1, Math.floor(rect.width * devicePixelRatio));
  canvas.height = Math.max(1, Math.floor(rect.height * devicePixelRatio));
}

function colorFor(node: SimNode, selected: boolean): string {
  if (selected) {
    return "#f7c948";
  }
  if (selectedId !== null && !isSelectedNeighbor(node)) {
    return hexToRgba(node.groupColor, 0.34);
  }
  return node.node.is_test ? hexToRgba(node.groupColor, 0.58) : hexToRgba(node.groupColor, 0.86);
}

function strokeFor(node: SimNode, selected: boolean): string {
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

function edgeColor(resolution: Resolution, alpha: number): string {
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

function shouldLabelNode(node: SimNode): boolean {
  return (
    node.node.id === selectedId ||
    (selectedId !== null && isSelectedNeighbor(node)) ||
    node.centrality >= labelCutoff ||
    (camera.zoom > 1.15 && node.radius >= 8)
  );
}

function isSelectedNeighbor(node: SimNode): boolean {
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

import {
  createGraphView,
  hiddenIncomingCallers,
  type FunctionGraphReport,
  type GraphOptions,
  type GraphView,
  type HiddenCaller,
  type NodeMetric,
  type Resolution,
} from "./graph";
import "./styles.css";
import { draw, drawEmpty, type RenderState } from "./viewer/canvas";
import { getCanvasContext, getElement, resizeCanvasToDisplaySize } from "./viewer/dom";
import { applyWheelZoom, computeFitCamera, nearestNode, screenToWorld } from "./viewer/interaction";
import { buildSimulation, simulateGraph } from "./viewer/layout";
import { renderDetails, renderNoData, renderPanels, type PanelElements } from "./viewer/panels";
import type { Camera, LayoutGroup, SimEdge, SimNode } from "./viewer/types";

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

const panels: PanelElements = {
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
let camera: Camera = { x: 0, y: 0, zoom: 1 };
let dragState:
  | { mode: "node"; node: SimNode }
  | { mode: "pan"; x: number; y: number; cameraX: number; cameraY: number }
  | null = null;

wireControls();
resizeCanvasToDisplaySize(canvas, devicePixelRatio);
window.addEventListener("resize", () => {
  resizeCanvasToDisplaySize(canvas, devicePixelRatio);
  fitView();
  redraw();
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
    renderNoData(panels);
    drawEmpty(context);
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
    redraw();
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
  const simulation = buildSimulation(nodes, view, options.metric);
  nodes = simulation.nodes;
  edges = simulation.edges;
  layoutGroups = simulation.layoutGroups;
  labelCutoff = simulation.labelCutoff;
  renderPanels(panels, report, view, options.metric, selectedId, selectNode);
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

function startSimulation(ticks: number): void {
  settleTicks = ticks;
  if (animationFrame === null) {
    animationFrame = requestAnimationFrame(step);
  }
}

function step(): void {
  animationFrame = null;
  if (settleTicks > 0) {
    simulateGraph(nodes, edges);
    settleTicks -= 1;
    animationFrame = requestAnimationFrame(step);
  }
  redraw();
}

function redraw(): void {
  draw(context, renderState());
}

function renderState(): RenderState {
  return {
    camera,
    edges,
    hiddenCallers: selectedHiddenCallers(),
    labelCutoff,
    layoutGroups,
    nodes,
    selectedId,
  };
}

function selectedHiddenCallers(): HiddenCaller[] {
  if (selectedId === null || view === null || report === null) {
    return [];
  }
  const selected = view.nodes.find((node) => node.id === selectedId);
  if (selected === undefined) {
    return [];
  }
  return hiddenIncomingCallers(report, selected, view);
}

function onPointerDown(event: MouseEvent): void {
  const point = pointerWorldPoint(event);
  const hit = nearestNode(nodes, point.x, point.y);
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
    const point = pointerWorldPoint(event);
    dragState.node.x = point.x;
    dragState.node.y = point.y;
    dragState.node.vx = 0;
    dragState.node.vy = 0;
    startSimulation(30);
    return;
  }
  camera.x = dragState.cameraX + event.clientX - dragState.x;
  camera.y = dragState.cameraY + event.clientY - dragState.y;
  redraw();
}

function onWheel(event: WheelEvent): void {
  event.preventDefault();
  camera.zoom = applyWheelZoom(camera.zoom, event.deltaY);
  redraw();
}

function pointerWorldPoint(event: MouseEvent): { x: number; y: number } {
  return screenToWorld(
    camera,
    canvas.width,
    canvas.height,
    devicePixelRatio,
    event.offsetX,
    event.offsetY,
  );
}

function selectNode(id: string | null): void {
  selectedId = id;
  if (view !== null) {
    renderDetails(panels, report, view, selectedId);
  }
  redraw();
}

function fitView(): void {
  camera = computeFitCamera(nodes, canvas.width, canvas.height);
}

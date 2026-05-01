export type Resolution = "resolved" | "unresolved" | "ambiguous" | "anonymous";

export interface FunctionGraphReport {
  schema_version: number;
  root: string;
  language: string;
  node_count: number;
  edge_count: number;
  nodes: GraphNode[];
  edges: GraphEdge[];
  summary: GraphSummary;
}

export interface GraphNode {
  id: string;
  name: string;
  qualified_name: string;
  file: string;
  module: string;
  impl_owner: string | null;
  start_line: number;
  end_line: number;
  is_test: boolean;
  weights: NodeWeights;
}

export interface NodeWeights {
  incoming_call_count: number;
  outgoing_call_count: number;
  fan_in: number;
  fan_out: number;
  loc: number;
  cyclomatic_complexity: number | null;
  cognitive_complexity: number | null;
  max_nesting: number | null;
  maintainability_index: number | null;
  halstead_volume: number | null;
  total_time_ms: number | null;
  self_time_ms: number | null;
  error_count: number | null;
}

export interface GraphEdge {
  from: string | null;
  to: string | null;
  callee_name: string | null;
  resolution: Resolution;
  call_count: number;
  call_lines: number[];
  weights: {
    call_count: number;
    total_transition_time_ms: number | null;
    error_count: number | null;
  };
}

export interface GraphSummary {
  resolved_edge_count: number;
  unresolved_edge_count: number;
  ambiguous_edge_count: number;
  anonymous_edge_count: number;
  total_static_call_count: number;
}

export type NodeMetric = "fan_in" | "fan_out" | "calls" | "complexity" | "loc" | "maintainability";

export interface GraphOptions {
  query: string;
  hideTests: boolean;
  maxNodes: number;
  minCalls: number;
  resolution: Resolution | "all";
  metric: NodeMetric;
}

export interface GraphView {
  nodes: GraphNode[];
  edges: GraphEdge[];
  nodeIds: Set<string>;
  unresolvedEdges: GraphEdge[];
  stats: {
    totalNodes: number;
    totalEdges: number;
    visibleNodes: number;
    visibleEdges: number;
    hiddenByLimit: number;
    unresolvedVisible: number;
  };
}

export type HiddenCaller = {
  node: GraphNode;
  callCount: number;
};

export type EdgeRow = {
  count: number;
  label: string;
};

const DEFAULT_OPTIONS: GraphOptions = {
  query: "",
  hideTests: false,
  maxNodes: 140,
  minCalls: 1,
  resolution: "all",
  metric: "calls",
};

export function withDefaultOptions(options: Partial<GraphOptions>): GraphOptions {
  return { ...DEFAULT_OPTIONS, ...options };
}

export function createGraphView(
  report: FunctionGraphReport,
  partialOptions: Partial<GraphOptions>,
): GraphView {
  const options = withDefaultOptions(partialOptions);
  const query = options.query.trim().toLowerCase();
  const queried = filterNodes(report.nodes, query, options.hideTests);
  const limited = limitNodes(rankNodes(queried, options.metric), options.maxNodes);
  const nodeIds = new Set(limited.map((node) => node.id));
  const edges = filterVisibleEdges(report.edges, nodeIds, options);
  const unresolvedEdges = collectUnresolvedEdges(report.edges, nodeIds, options);

  return {
    nodes: limited,
    edges,
    nodeIds,
    unresolvedEdges,
    stats: {
      totalNodes: report.node_count,
      totalEdges: report.edge_count,
      visibleNodes: limited.length,
      visibleEdges: edges.length,
      hiddenByLimit: Math.max(0, queried.length - limited.length),
      unresolvedVisible: unresolvedEdges.length,
    },
  };
}

export function hiddenIncomingCallers(
  report: FunctionGraphReport,
  selected: GraphNode,
  currentView: GraphView,
): HiddenCaller[] {
  const byId = new Map(report.nodes.map((node) => [node.id, node]));
  const callers = new Map<string, HiddenCaller>();
  for (const edge of report.edges) {
    if (
      edge.to !== selected.id ||
      edge.from === null ||
      edge.resolution !== "resolved" ||
      currentView.nodeIds.has(edge.from)
    ) {
      continue;
    }
    const node = byId.get(edge.from);
    if (node === undefined) {
      continue;
    }
    const current = callers.get(edge.from);
    if (current === undefined) {
      callers.set(edge.from, { node, callCount: edge.call_count });
    } else {
      current.callCount += edge.call_count;
    }
  }
  return [...callers.values()].sort(
    (a, b) =>
      b.callCount - a.callCount ||
      scoreNode(b.node, "calls") - scoreNode(a.node, "calls") ||
      a.node.name.localeCompare(b.node.name),
  );
}

export function edgeRows(
  edges: GraphEdge[],
  field: "from" | "to" | "callee_name",
  viewNodes: { id: string; name: string }[],
  count = 8,
): EdgeRow[] {
  return edges.slice(0, count).map((edge) => {
    const id = field === "callee_name" ? edge.callee_name : edge[field];
    return {
      count: edge.call_count,
      label:
        field === "callee_name"
          ? (id ?? "unknown")
          : (viewNodes.find((node) => node.id === id)?.name ?? id ?? "unknown"),
    };
  });
}

export function scoreNode(node: GraphNode, metric: NodeMetric): number {
  switch (metric) {
    case "fan_in":
      return node.weights.fan_in;
    case "fan_out":
      return node.weights.fan_out;
    case "complexity":
      return node.weights.cognitive_complexity ?? node.weights.cyclomatic_complexity ?? 0;
    case "loc":
      return node.weights.loc;
    case "maintainability":
      return node.weights.maintainability_index ?? 0;
    case "calls":
      return node.weights.incoming_call_count + node.weights.outgoing_call_count;
  }
}

export function topNodes(
  report: FunctionGraphReport,
  metric: NodeMetric,
  count: number,
): GraphNode[] {
  return [...report.nodes]
    .sort((a, b) => scoreNode(b, metric) - scoreNode(a, metric))
    .slice(0, count);
}

function filterNodes(nodes: GraphNode[], query: string, hideTests: boolean): GraphNode[] {
  return nodes.filter((node) => matchesNode(node, query, hideTests));
}

function rankNodes(nodes: GraphNode[], metric: NodeMetric): GraphNode[] {
  return [...nodes].sort((a, b) => scoreNode(b, metric) - scoreNode(a, metric));
}

function limitNodes(nodes: GraphNode[], maxNodes: number): GraphNode[] {
  return nodes.slice(0, maxNodes);
}

function filterVisibleEdges(
  edges: GraphEdge[],
  nodeIds: Set<string>,
  options: GraphOptions,
): GraphEdge[] {
  return edges.filter(
    (edge) =>
      edgeMatchesOptions(edge, options) &&
      edge.from !== null &&
      edge.to !== null &&
      nodeIds.has(edge.from) &&
      nodeIds.has(edge.to),
  );
}

function collectUnresolvedEdges(
  edges: GraphEdge[],
  nodeIds: Set<string>,
  options: GraphOptions,
): GraphEdge[] {
  return edges.filter(
    (edge) =>
      edgeMatchesOptions(edge, options) &&
      edge.from !== null &&
      nodeIds.has(edge.from) &&
      edge.to === null,
  );
}

function edgeMatchesOptions(edge: GraphEdge, options: GraphOptions): boolean {
  return (
    edge.call_count >= options.minCalls &&
    (options.resolution === "all" || edge.resolution === options.resolution)
  );
}

function matchesNode(node: GraphNode, query: string, hideTests: boolean): boolean {
  if (hideTests && node.is_test) {
    return false;
  }
  if (query.length === 0) {
    return true;
  }
  return (
    node.name.toLowerCase().includes(query) ||
    node.qualified_name.toLowerCase().includes(query) ||
    node.file.toLowerCase().includes(query)
  );
}

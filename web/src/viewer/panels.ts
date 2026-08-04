import {
  edgeRows,
  hiddenIncomingCallers,
  scoreNode,
  topNodes,
  type FunctionGraphReport,
  type GraphEdge,
  type GraphNode,
  type GraphView,
  type HiddenCaller,
  type NodeMetric,
} from "../graph";
import { escapeAttr, escapeHtml, formatNumber } from "./dom";
import { groupFor } from "./layout";

/** Panel elements the viewer writes into. */
export type PanelElements = {
  loadState: HTMLElement;
  stats: HTMLElement;
  ranking: HTMLElement;
  details: HTMLElement;
};

type ListRow = {
  label: string;
  count: number;
};

export function renderPanels(
  panels: PanelElements,
  report: FunctionGraphReport,
  view: GraphView,
  metric: NodeMetric,
  selectedId: string | null,
  onSelect: (id: string | null) => void,
): void {
  panels.stats.innerHTML = statsMarkup(view);
  panels.ranking.innerHTML = rankingMarkup(report, metric);
  for (const button of panels.ranking.querySelectorAll<HTMLButtonElement>("button[data-node-id]")) {
    button.addEventListener("click", () => onSelect(button.dataset.nodeId ?? null));
  }
  renderDetails(panels, report, view, selectedId);
}

export function renderDetails(
  panels: Pick<PanelElements, "details">,
  report: FunctionGraphReport | null,
  view: GraphView,
  selectedId: string | null,
): void {
  panels.details.innerHTML = detailsMarkup(report, view, selectedId);
}

export function renderNoData(panels: Pick<PanelElements, "loadState" | "details">): void {
  panels.loadState.textContent = "no data";
  panels.details.innerHTML = `
    <h2>No graph loaded</h2>
    <p>Generate <code>public/function-graph.json</code> or load an analyzer JSON file.</p>
  `;
}

export function statsMarkup(view: GraphView): string {
  return `
    <dl>
      <div><dt>Visible</dt><dd>${view.stats.visibleNodes} / ${view.stats.totalNodes}</dd></div>
      <div><dt>Edges</dt><dd>${view.stats.visibleEdges} / ${view.stats.totalEdges}</dd></div>
      <div><dt>Modules</dt><dd>${new Set(view.nodes.map(groupFor)).size}</dd></div>
      <div><dt>Hidden</dt><dd>${view.stats.hiddenByLimit}</dd></div>
      <div><dt>Unresolved</dt><dd>${view.stats.unresolvedVisible}</dd></div>
    </dl>
  `;
}

export function rankingMarkup(report: FunctionGraphReport, metric: NodeMetric): string {
  return `
    <h2>Top functions</h2>
    <ol>
      ${topNodes(report, metric, 8)
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
}

export function detailsMarkup(
  report: FunctionGraphReport | null,
  view: GraphView,
  selectedId: string | null,
): string {
  const selected =
    selectedId === null ? null : (view.nodes.find((node) => node.id === selectedId) ?? null);
  if (selected === null) {
    return `
      <h2>Inspect</h2>
      <p>Select a node to inspect calls, complexity, and source location.</p>
    `;
  }
  const outgoing = view.edges.filter((edge) => edge.from === selected.id);
  const incoming = view.edges.filter((edge) => edge.to === selected.id);
  const unresolved = view.unresolvedEdges.filter((edge) => edge.from === selected.id);
  const hiddenIncoming = report === null ? [] : hiddenIncomingCallers(report, selected, view);
  return `
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
      ${edgeSection("Outgoing", outgoing, "to", view.nodes)}
      ${edgeSection("Incoming", incoming, "from", view.nodes)}
      ${edgeSection("Unresolved", unresolved, "callee_name", view.nodes)}
      ${hiddenCallerSection(hiddenIncoming)}
    </div>
  `;
}

export function edgeSection(
  title: string,
  list: GraphEdge[],
  field: "from" | "to" | "callee_name",
  viewNodes: GraphNode[],
): string {
  return rowSection(title, edgeRows(list, field, viewNodes));
}

export function hiddenCallerSection(callers: HiddenCaller[]): string {
  return rowSection(
    "Hidden callers",
    callers.slice(0, 8).map((caller) => ({ label: caller.node.name, count: caller.callCount })),
  );
}

function rowSection(title: string, rows: ListRow[]): string {
  const items = rows.map(
    (row) => `<li><span>${escapeHtml(row.label)}</span><strong>${row.count}</strong></li>`,
  );
  return `
    <section>
      <h3>${title}</h3>
      <ul>${items.length > 0 ? items.join("") : "<li><span>none</span><strong>0</strong></li>"}</ul>
    </section>
  `;
}

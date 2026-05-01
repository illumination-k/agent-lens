import {
  edgeRows,
  scoreNode,
  topNodes,
  type FunctionGraphReport,
  type GraphEdge,
  type GraphView,
} from "../graph";

type FunctionGraphShellProps = {
  initialView: GraphView | null;
  report: FunctionGraphReport | null;
};

export function FunctionGraphShell({ initialView, report }: FunctionGraphShellProps) {
  const firstNode = initialView?.nodes[0] ?? null;
  const outgoing =
    firstNode === null
      ? []
      : (initialView?.edges.filter((edge) => edge.from === firstNode.id) ?? []);
  const incoming =
    firstNode === null ? [] : (initialView?.edges.filter((edge) => edge.to === firstNode.id) ?? []);
  const unresolved =
    firstNode === null
      ? []
      : (initialView?.unresolvedEdges.filter((edge) => edge.from === firstNode.id) ?? []);

  return (
    <div id="app">
      <main className="shell">
        <Sidebar initialView={initialView} report={report} />
        <section className="stage" aria-label="Function graph viewer">
          <canvas id="graph"></canvas>
          <div className="toolbar" aria-label="View controls">
            <button id="fit" type="button">
              Fit
            </button>
            <button id="restart" type="button">
              Settle
            </button>
          </div>
          <section id="details" className="details" aria-label="Function details">
            {firstNode === null ? (
              <EmptyDetails />
            ) : (
              <InitialDetails
                incoming={incoming}
                initialView={initialView}
                node={firstNode}
                outgoing={outgoing}
                unresolved={unresolved}
              />
            )}
          </section>
        </section>
      </main>
    </div>
  );
}

function Sidebar({ initialView, report }: FunctionGraphShellProps) {
  return (
    <aside className="sidebar" aria-label="Graph controls">
      <div className="brand">
        <div>
          <p className="eyebrow">agent-lens</p>
          <h1>Function graph</h1>
        </div>
        <span id="load-state" className="status">
          {report === null ? "no data" : "ssg"}
        </span>
      </div>

      <label className="field">
        <span>Search</span>
        <input id="query" type="search" placeholder="function, module, file" autoComplete="off" />
      </label>

      <div className="field two">
        <label>
          <span>Metric</span>
          <select id="metric">
            <option value="calls">Calls</option>
            <option value="fan_in">Fan in</option>
            <option value="fan_out">Fan out</option>
            <option value="complexity">Complexity</option>
            <option value="loc">LOC</option>
            <option value="maintainability">Maintainability</option>
          </select>
        </label>
        <label>
          <span>Edges</span>
          <select id="resolution">
            <option value="all">All</option>
            <option value="resolved">Resolved</option>
            <option value="unresolved">Unresolved</option>
            <option value="ambiguous">Ambiguous</option>
            <option value="anonymous">Anonymous</option>
          </select>
        </label>
      </div>

      <label className="field">
        <span>
          Max nodes <output id="max-nodes-value">140</output>
        </span>
        <input id="max-nodes" type="range" min="20" max="420" step="10" defaultValue="140" />
      </label>

      <label className="field">
        <span>
          Min calls <output id="min-calls-value">1</output>
        </span>
        <input id="min-calls" type="range" min="1" max="10" step="1" defaultValue="1" />
      </label>

      <label className="check">
        <input id="hide-tests" type="checkbox" />
        <span>Hide tests</span>
      </label>

      <label className="upload">
        <input id="upload" type="file" accept="application/json,.json" />
        <span>Load JSON</span>
      </label>

      <Stats initialView={initialView} />
      <Ranking report={report} />
    </aside>
  );
}

function Stats({ initialView }: { initialView: GraphView | null }) {
  return (
    <section id="stats" className="stats" aria-label="Graph stats">
      {initialView === null ? (
        <p>
          Generate <code>web/public/function-graph.json</code> before building.
        </p>
      ) : (
        <dl>
          <div>
            <dt>Visible</dt>
            <dd>
              {initialView.stats.visibleNodes} / {initialView.stats.totalNodes}
            </dd>
          </div>
          <div>
            <dt>Edges</dt>
            <dd>
              {initialView.stats.visibleEdges} / {initialView.stats.totalEdges}
            </dd>
          </div>
          <div>
            <dt>Hidden</dt>
            <dd>{initialView.stats.hiddenByLimit}</dd>
          </div>
          <div>
            <dt>Unresolved</dt>
            <dd>{initialView.stats.unresolvedVisible}</dd>
          </div>
        </dl>
      )}
    </section>
  );
}

function Ranking({ report }: { report: FunctionGraphReport | null }) {
  return (
    <section id="ranking" className="ranking" aria-label="Top functions">
      <h2>Top functions</h2>
      <ol>
        {report !== null &&
          topNodes(report, "calls", 8).map((node) => (
            <li key={node.id}>
              <button type="button" data-node-id={node.id}>
                <span>{node.name}</span>
                <strong>{scoreNode(node, "calls").toFixed(0)}</strong>
              </button>
            </li>
          ))}
      </ol>
    </section>
  );
}

function EmptyDetails() {
  return (
    <>
      <h2>No graph loaded</h2>
      <p>
        Generate <code>web/public/function-graph.json</code> or load an analyzer JSON file.
      </p>
    </>
  );
}

function InitialDetails({
  incoming,
  initialView,
  node,
  outgoing,
  unresolved,
}: {
  incoming: GraphEdge[];
  initialView: GraphView | null;
  node: GraphView["nodes"][number];
  outgoing: GraphEdge[];
  unresolved: GraphEdge[];
}) {
  return (
    <>
      <h2>{node.name}</h2>
      <p className="qualified">{node.qualified_name}</p>
      <dl className="detail-grid">
        <div>
          <dt>File</dt>
          <dd>
            {node.file}:{node.start_line}
          </dd>
        </div>
        <div>
          <dt>Fan</dt>
          <dd>
            {node.weights.fan_in} in / {node.weights.fan_out} out
          </dd>
        </div>
        <div>
          <dt>Calls</dt>
          <dd>
            {node.weights.incoming_call_count} in / {node.weights.outgoing_call_count} out
          </dd>
        </div>
        <div>
          <dt>Complexity</dt>
          <dd>{node.weights.cognitive_complexity ?? "n/a"}</dd>
        </div>
        <div>
          <dt>LOC</dt>
          <dd>{node.weights.loc}</dd>
        </div>
        <div>
          <dt>MI</dt>
          <dd>{node.weights.maintainability_index?.toFixed(1) ?? "n/a"}</dd>
        </div>
      </dl>
      <div className="columns">
        <EdgeList
          edges={outgoing}
          field="to"
          title="Outgoing"
          viewNodes={initialView?.nodes ?? []}
        />
        <EdgeList
          edges={incoming}
          field="from"
          title="Incoming"
          viewNodes={initialView?.nodes ?? []}
        />
        <EdgeList
          edges={unresolved}
          field="callee_name"
          title="Unresolved"
          viewNodes={initialView?.nodes ?? []}
        />
      </div>
    </>
  );
}

function EdgeList({
  edges,
  field,
  title,
  viewNodes,
}: {
  edges: GraphEdge[];
  field: "from" | "to" | "callee_name";
  title: string;
  viewNodes: { id: string; name: string }[];
}) {
  const rows = edgeRows(edges, field, viewNodes);
  return (
    <section>
      <h3>{title}</h3>
      <ul>
        {rows.length > 0 ? (
          rows.map((edge) => (
            <li key={`${edge.label}-${edge.count}`}>
              <span>{edge.label}</span>
              <strong>{edge.count}</strong>
            </li>
          ))
        ) : (
          <li>
            <span>none</span>
            <strong>0</strong>
          </li>
        )}
      </ul>
    </section>
  );
}

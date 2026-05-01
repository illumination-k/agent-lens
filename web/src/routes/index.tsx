import { createFileRoute } from "@tanstack/react-router";
import { useEffect } from "react";

import { FunctionGraphShell } from "../components/FunctionGraphShell";
import { createGraphView, type FunctionGraphReport } from "../graph";

type LoaderData = {
  report: FunctionGraphReport | null;
};

export const Route = createFileRoute("/")({
  component: FunctionGraphPage,
  loader: loadGraphReport,
});

async function loadGraphReport(): Promise<LoaderData> {
  if (import.meta.env.SSR) {
    const { readGraphReportFromDisk } = await import("../server/graphReport");
    return { report: readGraphReportFromDisk() };
  }
  const response = await fetch(`${import.meta.env.BASE_URL}function-graph.json`, {
    cache: "no-store",
  });
  return {
    report: response.ok ? ((await response.json()) as FunctionGraphReport) : null,
  };
}

function FunctionGraphPage() {
  const { report } = Route.useLoaderData();
  const initialView =
    report === null
      ? null
      : createGraphView(report, {
          metric: "calls",
          maxNodes: 140,
          minCalls: 1,
        });
  const embeddedReport = report === null ? "" : JSON.stringify(report).replaceAll("<", "\\u003c");

  useEffect(() => {
    void import("../main");
  }, []);

  return (
    <>
      {report !== null && (
        <script
          id="function-graph-data"
          type="application/json"
          dangerouslySetInnerHTML={{ __html: embeddedReport }}
        />
      )}
      <FunctionGraphShell initialView={initialView} report={report} />
    </>
  );
}

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import type { FunctionGraphReport } from "../graph";

export function readGraphReportFromDisk(): FunctionGraphReport | null {
  const reportPath = join(process.cwd(), "public", "function-graph.json");
  if (!existsSync(reportPath)) {
    return null;
  }
  return JSON.parse(readFileSync(reportPath, "utf8")) as FunctionGraphReport;
}

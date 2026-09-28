/**
 * Numbers behind the call-graph accuracy article.
 *
 * Accuracy is `mise run callgraph-accuracy` on agent-lens `2a26836`
 * (`target/callgraph-accuracy/results/<target>/score.json`); the method
 * behind it is `docs/callgraph-accuracy.md`. Wall-clock is the median of 10
 * runs of `agent-lens analyze function-graph` and of 3 runs of each oracle,
 * warm caches, on a 4-vCPU Xeon @ 2.10GHz. Rerun both when the resolver
 * changes rather than editing one row.
 */

export const MEASURED_AT = "2026-09-28";
export const MEASURED_COMMIT = "2a26836";

export type TargetRow = {
  target: string;
  version: string;
  language: "Go" | "Python" | "Rust" | "TypeScript";
  oracle: string;
  /** Resolved agent-lens pairs confirmed by the oracle / resolved pairs in its view. */
  tp: number;
  resolved: number;
  /** Oracle static-dispatch pairs agent-lens resolved / all of them. */
  found: number;
  staticPairs: number;
  /** Static pairs agent-lens kept as an ambiguous candidate set instead of resolving. */
  onlyCandidate: number;
  nodes: number;
  edges: number;
  agentLensSeconds: number;
  oracleSeconds: number;
};

export const TARGETS: readonly TargetRow[] = [
  {
    target: "spf13/pflag",
    version: "v1.0.10",
    language: "Go",
    oracle: "go-vta",
    tp: 1098,
    resolved: 1115,
    found: 1094,
    staticPairs: 1301,
    onlyCandidate: 30,
    nodes: 925,
    edges: 2599,
    agentLensSeconds: 0.106,
    oracleSeconds: 1.76,
  },
  {
    target: "google/go-cmp",
    version: "v0.7.0",
    language: "Go",
    oracle: "go-vta",
    tp: 376,
    resolved: 376,
    found: 376,
    staticPairs: 453,
    onlyCandidate: 63,
    nodes: 404,
    edges: 1436,
    agentLensSeconds: 0.154,
    oracleSeconds: 1.74,
  },
  {
    target: "more-itertools",
    version: "v11.1.0",
    language: "Python",
    oracle: "pyright",
    tp: 885,
    resolved: 886,
    found: 885,
    staticPairs: 911,
    onlyCandidate: 6,
    nodes: 1004,
    edges: 3971,
    agentLensSeconds: 0.147,
    oracleSeconds: 101.02,
  },
  {
    target: "pytoolz/toolz",
    version: "1.1.0",
    language: "Python",
    oracle: "pyright",
    tp: 278,
    resolved: 286,
    found: 272,
    staticPairs: 281,
    onlyCandidate: 8,
    nodes: 379,
    edges: 1053,
    agentLensSeconds: 0.033,
    oracleSeconds: 25.32,
  },
  {
    target: "dtolnay/semver",
    version: "1.0.28",
    language: "Rust",
    oracle: "rust-analyzer",
    tp: 157,
    resolved: 157,
    found: 157,
    staticPairs: 206,
    onlyCandidate: 13,
    nodes: 139,
    edges: 466,
    agentLensSeconds: 0.027,
    oracleSeconds: 13.09,
  },
  {
    target: "rust-lang/log",
    version: "0.4.34",
    language: "Rust",
    oracle: "rust-analyzer",
    tp: 144,
    resolved: 181,
    found: 131,
    staticPairs: 267,
    onlyCandidate: 85,
    nodes: 388,
    edges: 747,
    agentLensSeconds: 0.042,
    oracleSeconds: 19.22,
  },
  {
    target: "supermacro/neverthrow",
    version: "v8.2.0",
    language: "TypeScript",
    oracle: "tsc",
    tp: 699,
    resolved: 699,
    found: 698,
    staticPairs: 864,
    onlyCandidate: 148,
    nodes: 681,
    edges: 1816,
    agentLensSeconds: 0.03,
    oracleSeconds: 1.51,
  },
  {
    target: "pmndrs/zustand",
    version: "v5.0.15",
    language: "TypeScript",
    oracle: "tsc",
    tp: 220,
    resolved: 225,
    found: 220,
    staticPairs: 226,
    onlyCandidate: 0,
    nodes: 1378,
    edges: 2573,
    agentLensSeconds: 0.05,
    oracleSeconds: 1.89,
  },
];

export type ProgressRow = {
  language: string;
  targets: string;
  precisionBefore: string;
  precisionAfter: string;
  recallBefore: string;
  recallAfter: string;
};

/**
 * Where each language started (the first run against its oracle, recorded in
 * `docs/callgraph-accuracy.md`) and where it is now, on the same two targets.
 */
export const PROGRESS: readonly ProgressRow[] = [
  {
    language: "Go",
    targets: "pflag + go-cmp",
    precisionBefore: "0.973",
    precisionAfter: "0.989",
    recallBefore: "0.536",
    recallAfter: "0.838",
  },
  {
    language: "Python",
    targets: "more-itertools + toolz",
    precisionBefore: "0.810",
    precisionAfter: "0.992",
    recallBefore: "0.313",
    recallAfter: "0.971",
  },
  {
    language: "Rust",
    targets: "semver + log",
    precisionBefore: "0.924",
    precisionAfter: "0.891",
    recallBefore: "0.550",
    recallAfter: "0.609",
  },
  {
    language: "TypeScript",
    targets: "neverthrow + zustand",
    precisionBefore: "0.995",
    precisionAfter: "0.995",
    recallBefore: "0.704",
    recallAfter: "0.842",
  },
];

export function ratio(numerator: number, denominator: number): string {
  return denominator === 0 ? "n/a" : (numerator / denominator).toFixed(3);
}

export type Totals = Pick<
  TargetRow,
  "tp" | "resolved" | "found" | "staticPairs" | "agentLensSeconds" | "oracleSeconds"
>;

/** Micro-averaged: counts are summed before dividing, as `run.py` does. */
export function totals(rows: readonly TargetRow[]): Totals {
  return rows.reduce<Totals>(
    (sum, row) => ({
      tp: sum.tp + row.tp,
      resolved: sum.resolved + row.resolved,
      found: sum.found + row.found,
      staticPairs: sum.staticPairs + row.staticPairs,
      agentLensSeconds: sum.agentLensSeconds + row.agentLensSeconds,
      oracleSeconds: sum.oracleSeconds + row.oracleSeconds,
    }),
    { tp: 0, resolved: 0, found: 0, staticPairs: 0, agentLensSeconds: 0, oracleSeconds: 0 },
  );
}

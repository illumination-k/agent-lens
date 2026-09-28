import { describe, expect, it } from "vitest";

import { TARGETS, ratio, totals } from "./articleData";
import { PAGES } from "./seo";

describe("articleData", () => {
  it("micro-averages by summing counts before dividing", () => {
    const all = totals(TARGETS);
    expect(ratio(all.tp, all.resolved)).toBe("0.983");
    expect(ratio(all.found, all.staticPairs)).toBe("0.850");
  });

  it("keeps the page description in step with the table", () => {
    const all = totals(TARGETS);
    const speedup = Math.round(all.oracleSeconds / all.agentLensSeconds / 10) * 10;
    const { description } = PAGES.callGraphAccuracy;
    expect(description).toContain(`${(all.tp / all.resolved).toFixed(2)} precision`);
    expect(description).toContain(`${(all.found / all.staticPairs).toFixed(2)} recall`);
    expect(description).toContain(`${speedup}x faster`);
  });

  it.each(TARGETS)("never confirms more pairs than $target resolved", (row) => {
    expect(row.tp).toBeLessThanOrEqual(row.resolved);
    expect(row.found + row.onlyCandidate).toBeLessThanOrEqual(row.staticPairs);
  });

  it("guards the ratio against an empty denominator", () => {
    expect(ratio(1, 0)).toBe("n/a");
  });
});

import { describe, expect, it } from "vitest";
import { makeEdge, makeNode } from "../testSupport";
import { edgeSection, hiddenCallerSection } from "./panels";

describe("edgeSection", () => {
  it("renders one row per edge with the target name and call count", () => {
    const nodes = [makeNode("a", "load", "crate::load", false, {})];
    const html = edgeSection("Outgoing", [makeEdge("b", "a", "resolved", 3)], "to", nodes);
    expect(html).toContain("<h3>Outgoing</h3>");
    expect(html).toContain("<li><span>load</span><strong>3</strong></li>");
  });

  it("renders a placeholder row when the list is empty", () => {
    const html = edgeSection("Incoming", [], "from", []);
    expect(html).toContain("<li><span>none</span><strong>0</strong></li>");
  });

  it("escapes HTML in labels", () => {
    const nodes = [makeNode("a", "<script>", "crate::x", false, {})];
    const html = edgeSection("Outgoing", [makeEdge("b", "a", "resolved", 1)], "to", nodes);
    expect(html).toContain("&lt;script&gt;");
    expect(html).not.toContain("<script>");
  });
});

describe("hiddenCallerSection", () => {
  it("renders callers with their call counts, capped at eight rows", () => {
    const callers = Array.from({ length: 10 }, (_, index) => ({
      node: makeNode(`n${index}`, `caller${index}`, `crate::caller${index}`, false, {}),
      callCount: index + 1,
    }));
    const html = hiddenCallerSection(callers);
    expect(html).toContain("<h3>Hidden callers</h3>");
    expect(html).toContain("<li><span>caller0</span><strong>1</strong></li>");
    expect(html).toContain("caller7");
    expect(html).not.toContain("caller8");
  });

  it("renders a placeholder row when there are no hidden callers", () => {
    expect(hiddenCallerSection([])).toContain("<li><span>none</span><strong>0</strong></li>");
  });
});

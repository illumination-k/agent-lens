// Tests for oracle.mjs on a tiny project covering the call shapes it must
// handle: `npm ci && npm test` in this directory.

import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";

import { run } from "./oracle.mjs";

const A = `/** doc */
export function helper(x: number): number {
  return x + 1;
}

export interface Shape {
  area(): number;
}

export class Sq implements Shape {
  constructor(private v: number) {}
  area(): number {
    return helper(this.v);
  }
  @dec
  double(): number {
    return this.area() * 2;
  }
}

function dec(a: any, b: any, c: any) { return c; }

export const arrow = (x: number) => helper(x);

export function over(x: string): string;
export function over(x: number): number;
export function over(x: any): any {
  return x;
}

export function run(s: Shape): number {
  const f = helper;
  const sq = new Sq(3);
  over(1);
  return f(1) + s.area() + sq.double() + arrow(1) + use(() => helper(2));
}

function use(cb: () => number): number {
  return cb();
}

export class Circle {
  area(): number {
    return 3;
  }
}

export function either(s: Sq | Circle): number {
  return s.area();
}
`;

const B = `import { run as r, Sq } from "./a";
export function main() {
  return r(new Sq(1));
}
main();
`;

let dir;
let edges;
let doc;

before(() => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "oracle-ts-"));
  fs.writeFileSync(path.join(dir, "a.ts"), A);
  fs.writeFileSync(path.join(dir, "b.ts"), B);
  fs.writeFileSync(path.join(dir, "tsconfig.json"), JSON.stringify({ compilerOptions: { experimentalDecorators: true, strict: true } }));
  ({ doc } = run(dir));
  edges = new Set(doc.edges.map((e) => `${e.caller_file}:${e.caller_def_line}@${e.call_line} -> ${e.callee_file}:${e.callee_def_line} ${e.dispatch}`));
});

after(() => fs.rmSync(dir, { recursive: true, force: true }));

test("direct function and method calls are static", () => {
  assert.ok(edges.has("a.ts:12@13 -> a.ts:2 static")); // Sq.area -> helper
  assert.ok(edges.has("a.ts:15@17 -> a.ts:12 static")); // this.area()
  assert.ok(edges.has("a.ts:23@23 -> a.ts:2 static")); // const arrow -> helper
  assert.ok(edges.has("a.ts:31@35 -> a.ts:15 static")); // sq.double(): decorated method, def line on the decorator
  assert.ok(edges.has("a.ts:31@35 -> a.ts:23 static")); // arrow(1): a const binding
});

test("import aliases and constructors", () => {
  assert.ok(edges.has("b.ts:2@3 -> a.ts:31 static")); // r(...) is run
  assert.ok(edges.has("b.ts:2@3 -> a.ts:11 static")); // new Sq(1) -> constructor
  assert.ok(edges.has("a.ts:31@33 -> a.ts:11 static"));
});

test("an overload call maps to the implementation", () => {
  assert.ok(edges.has("a.ts:31@34 -> a.ts:27 static"));
});

test("calls through a value are dynamic", () => {
  assert.ok(edges.has("a.ts:31@35 -> a.ts:2 dynamic")); // f(1), f = helper
});

test("an interface method call is dynamic to every implementing class", () => {
  assert.ok(edges.has("a.ts:31@35 -> a.ts:12 dynamic")); // s.area()
});

test("a method call on a union receiver is dynamic to every member's method", () => {
  assert.ok(edges.has("a.ts:48@49 -> a.ts:12 dynamic"));
  assert.ok(edges.has("a.ts:48@49 -> a.ts:43 dynamic"));
  assert.ok(![...edges].some((e) => e.startsWith("a.ts:48@49") && e.endsWith("static")));
});

test("the caller is the innermost function, anonymous ones included", () => {
  assert.equal(doc.caller_is_innermost_function, true);
  assert.ok(edges.has("a.ts:35@35 -> a.ts:2 static")); // helper(2) inside the arrow passed to use
  assert.ok(edges.has("a.ts:31@35 -> a.ts:38 static")); // use(...) itself belongs to run
});

test("module top-level calls are dropped and every file is analysed", () => {
  assert.ok(![...edges].some((e) => e.startsWith("b.ts:") && e.includes("@5 ")));
  assert.deepEqual(doc.analyzed_files, ["a.ts", "b.ts"]);
});

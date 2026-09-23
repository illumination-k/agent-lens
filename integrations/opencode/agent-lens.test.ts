// Drives the plugin against a stub `agent-lens` that records every call
// and answers from canned responses, so these tests pin the translation
// layer — payloads, routing, and what reaches opencode — not the analysis.

import assert from "node:assert/strict";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, it } from "node:test";

import plugin from "./agent-lens.ts";

const STUB = `#!/bin/sh
dir=$(dirname "$0")
payload=$(cat)
printf '%s\\t%s\\n' "$*" "$payload" >> "$dir/calls.log"
response="$dir/$1_$2_$3.json"
if [ -f "$response" ]; then cat "$response"; else echo '{}'; fi
`;

type Call = { args: string; payload: Record<string, any> };

let dir: string;
let prompts: unknown[];
let toasts: unknown[];

function respond(args: string, body: unknown) {
  writeFileSync(join(dir, `${args.replaceAll(" ", "_")}.json`), JSON.stringify(body));
}

function calls(): Call[] {
  let log: string;
  try {
    log = readFileSync(join(dir, "calls.log"), "utf8");
  } catch {
    return [];
  }
  return log
    .trim()
    .split("\n")
    .map((line) => {
      const [args, payload] = line.split("\t");
      return { args, payload: JSON.parse(payload) };
    });
}

async function load(env: Record<string, string> = {}) {
  process.env.AGENT_LENS_BIN = join(dir, "agent-lens");
  delete process.env.AGENT_LENS_OPENCODE_SKIP;
  Object.assign(process.env, env);
  return plugin.server({
    directory: dir,
    client: {
      session: { promptAsync: async (o) => void prompts.push(o) },
      tui: { showToast: async (o) => void toasts.push(o) },
    },
  });
}

async function edit(hooks: Awaited<ReturnType<typeof load>>, tool: string, args: object) {
  const ids = { tool, sessionID: "ses_1", callID: "call_1" };
  await hooks["tool.execute.before"](ids, { args: { ...args } });
  const output = { output: "done" };
  await hooks["tool.execute.after"]({ ...ids, args: { ...args } }, output);
  return output.output;
}

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "agent-lens-opencode-"));
  writeFileSync(join(dir, "agent-lens"), STUB);
  chmodSync(join(dir, "agent-lens"), 0o755);
  prompts = [];
  toasts = [];
});

afterEach(() => rmSync(dir, { recursive: true, force: true }));

describe("edit tools", () => {
  it("routes write/edit to the Claude Code tree with a repo-relative path", async () => {
    respond("hook pre-tool-use complexity", { systemMessage: "pre report" });
    respond("hook post-tool-use wrapper", { systemMessage: "post report" });
    const out = await edit(await load(), "edit", { filePath: join(dir, "src/lib.rs") });

    assert.equal(out, "done\n\npre report\n\npost report");
    const seen = calls();
    assert.deepEqual(seen.map((c) => c.args).sort(), [
      "hook post-tool-use footprint",
      "hook post-tool-use similarity",
      "hook post-tool-use wrapper",
      "hook pre-tool-use cohesion",
      "hook pre-tool-use complexity",
    ]);
    const pre = seen.find((c) => c.args === "hook pre-tool-use complexity")!.payload;
    assert.equal(pre.hook_event_name, "PreToolUse");
    assert.equal(pre.tool_name, "Edit");
    assert.deepEqual(pre.tool_input, { file_path: "src/lib.rs" });
    const post = seen.find((c) => c.args === "hook post-tool-use wrapper")!.payload;
    assert.equal(post.hook_event_name, "PostToolUse");
    assert.deepEqual(post.tool_response, {});
  });

  it("maps write to Claude Code's Write tool", async () => {
    await edit(await load(), "write", { filePath: "new.rs", content: "" });
    assert.ok(calls().every((c) => c.payload.tool_name === "Write"));
  });

  it("routes apply_patch to the Codex tree with the patch as the command", async () => {
    respond("codex-hook post-tool-use similarity", {
      hookSpecificOutput: { additionalContext: "codex report" },
    });
    const patchText = "*** Begin Patch\n*** Update File: a.rs\n*** End Patch\n";
    const out = await edit(await load(), "apply_patch", { patchText });

    assert.equal(out, "done\n\ncodex report");
    for (const { args, payload } of calls()) {
      assert.match(args, /^codex-hook /);
      assert.equal(payload.tool_name, "apply_patch");
      assert.deepEqual(payload.tool_input, { command: patchText });
      assert.equal(payload.tool_use_id, "call_1");
      assert.equal(typeof payload.model, "string");
    }
  });

  it("leaves other tools alone", async () => {
    const out = await edit(await load(), "read", { filePath: "a.rs" });
    assert.equal(out, "done");
    assert.deepEqual(calls(), []);
  });

  it("honours AGENT_LENS_OPENCODE_SKIP ids and bare events", async () => {
    const hooks = await load({ AGENT_LENS_OPENCODE_SKIP: "pre-tool-use, post-tool-use:footprint" });
    await edit(hooks, "edit", { filePath: "a.rs" });
    assert.deepEqual(
      calls()
        .map((c) => c.args)
        .sort(),
      ["hook post-tool-use similarity", "hook post-tool-use wrapper"],
    );
  });

  it("never fails the tool call when the binary is missing", async () => {
    const hooks = await load({ AGENT_LENS_BIN: join(dir, "missing") });
    const original = console.error;
    console.error = () => {};
    try {
      assert.equal(await edit(hooks, "edit", { filePath: "a.rs" }), "done");
    } finally {
      console.error = original;
    }
  });
});

describe("session checkpoint", () => {
  const created = (id: string, parentID?: string) => ({
    event: { type: "session.created", properties: { info: { id, parentID } } },
  });
  const idle = (sessionID: string) => ({
    event: { type: "session.idle", properties: { sessionID } },
  });

  it("snapshots, then injects the summary into the system prompt", async () => {
    respond("hook session-start summary", {
      hookSpecificOutput: { hookEventName: "SessionStart", additionalContext: "summary" },
    });
    const hooks = await load();
    await hooks.event(created("ses_1"));
    const output = { system: ["base"] };
    await hooks["experimental.chat.system.transform"]({ sessionID: "ses_1" }, output);

    assert.deepEqual(output.system, ["base", "summary"]);
    assert.deepEqual(
      calls().map((c) => c.args),
      ["hook session-start snapshot", "hook session-start summary"],
    );
    assert.equal(calls()[0].payload.session_id, "ses_1");

    // The summary is computed once per session, not once per LLM call.
    await hooks["experimental.chat.system.transform"]({ sessionID: "ses_1" }, { system: [] });
    assert.equal(calls().length, 2);
  });

  it("skips the checkpoint for child sessions", async () => {
    const hooks = await load();
    await hooks.event(created("ses_child", "ses_1"));
    const output = { system: [] };
    await hooks["experimental.chat.system.transform"]({ sessionID: "ses_child" }, output);
    await hooks.event(idle("ses_child"));
    assert.deepEqual(output.system, []);
    assert.deepEqual(calls(), []);
  });

  it("turns a blocking stop into one follow-up prompt", async () => {
    respond("hook stop delta", { decision: "block", reason: "regressions" });
    const hooks = await load();
    await hooks.event(idle("ses_1"));
    // The handler answers a continuation with a message, not a block.
    respond("hook stop delta", { systemMessage: "regressions" });
    await hooks.event(idle("ses_1"));

    assert.deepEqual(prompts, [
      { path: { id: "ses_1" }, body: { parts: [{ type: "text", text: "regressions" }] } },
    ]);
    // The second stop is the continuation the first one asked for.
    assert.deepEqual(
      calls().map((c) => c.payload.stop_hook_active),
      [false, true],
    );
  });

  it("shows a non-blocking stop report as a toast", async () => {
    respond("hook stop delta", { systemMessage: "seen before" });
    const hooks = await load();
    await hooks.event(idle("ses_1"));
    assert.deepEqual(prompts, []);
    assert.equal(toasts.length, 1);
  });
});

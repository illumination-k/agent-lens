// agent-lens plugin for opencode.
//
// Bridges opencode's plugin hooks onto the `agent-lens hook` (Claude Code)
// and `agent-lens codex-hook` handler trees, so the analysis lives in one
// place — the binary — and this file only translates payloads.
//
//   session start     -> `session-start snapshot` + `summary` (system prompt)
//   write / edit      -> `hook pre-tool-use` / `hook post-tool-use`
//   apply_patch       -> `codex-hook pre-tool-use` / `codex-hook post-tool-use`
//   session.idle      -> `hook stop delta`; a block becomes a follow-up prompt
//
// opencode has no channel for injecting context before a tool runs, so the
// pre-edit report is computed (and awaited, so it reads the file before the
// edit lands) in `tool.execute.before`, then held until the tool finishes and
// appended to the tool output together with the post-edit report. Child
// (subagent) sessions get the per-edit reports only; the checkpoint runs on
// the root session.
//
// Like the hooks it wraps, the plugin is advisory: a failing handler is
// logged and never fails the tool call or the session.
//
// Environment:
//   AGENT_LENS_BIN            binary to run (default: `agent-lens` on PATH)
//   AGENT_LENS_OPENCODE_SKIP  comma-separated hook ids or events to disable,
//                             e.g. `post-tool-use:footprint,stop`
//
// Only the default export is exported: opencode treats every other function
// export of a plugin module as a plugin of its own.

import { spawn } from "node:child_process";
import { isAbsolute, relative } from "node:path";

type HookId =
  | "session-start:summary"
  | "session-start:snapshot"
  | "pre-tool-use:complexity"
  | "pre-tool-use:cohesion"
  | "post-tool-use:similarity"
  | "post-tool-use:wrapper"
  | "post-tool-use:footprint"
  | "stop:delta";

const PRE_HOOKS: HookId[] = ["pre-tool-use:complexity", "pre-tool-use:cohesion"];
const POST_HOOKS: HookId[] = [
  "post-tool-use:similarity",
  "post-tool-use:wrapper",
  "post-tool-use:footprint",
];

/** Upper bound for one handler run; matches Claude Code's hook default. */
const HANDLER_TIMEOUT_MS = 60_000;

/** The slice of opencode's plugin input this plugin reads. */
type PluginInput = {
  directory: string;
  client: {
    session: {
      promptAsync(options: {
        path: { id: string };
        body: { parts: { type: "text"; text: string }[] };
      }): Promise<unknown>;
    };
    tui: {
      showToast(options: {
        body: { title?: string; message: string; variant: "info" | "warning" };
      }): Promise<unknown>;
    };
  };
};

type OpencodeEvent = {
  type: string;
  properties: {
    sessionID?: string;
    info?: { id: string; parentID?: string };
  };
};

/** The response fields the wrapped handlers answer through. */
type HookResponse = {
  systemMessage?: string;
  decision?: string;
  reason?: string;
  hookSpecificOutput?: { additionalContext?: string };
};

/** One handler invocation: the command tree plus the stdin payload. */
type Invocation = { tree: "hook" | "codex-hook"; payload: Record<string, unknown> };

type Run = (args: string[], stdin: string) => Promise<string>;

function spawnRun(bin: string, cwd: string): Run {
  return (args, stdin) =>
    new Promise((resolve, reject) => {
      const child = spawn(bin, args, { cwd, stdio: ["pipe", "pipe", "pipe"] });
      // Not spawn's `timeout` option: its timer outlives a child that never
      // started (e.g. ENOENT), holding the process open for the full minute.
      const timer = setTimeout(() => child.kill(), HANDLER_TIMEOUT_MS);
      let stdout = "";
      let stderr = "";
      child.stdout.on("data", (chunk) => (stdout += chunk));
      child.stderr.on("data", (chunk) => (stderr += chunk));
      child.stdin.on("error", () => {}); // EPIPE when the child exits early
      child.on("error", (err) => {
        clearTimeout(timer);
        reject(err);
      });
      child.on("close", (code) => {
        clearTimeout(timer);
        if (code === 0) resolve(stdout);
        else reject(new Error(`${bin} ${args.join(" ")} exited ${code}: ${stderr.trim()}`));
      });
      child.stdin.end(stdin);
    });
}

function parseSkip(raw: string | undefined): Set<string> {
  return new Set(
    (raw ?? "")
      .split(",")
      .map((s) => s.trim())
      .filter(Boolean),
  );
}

/** The text a handler meant for the model, whichever field carries it. */
function messageOf(response: HookResponse): string | undefined {
  return (
    response.hookSpecificOutput?.additionalContext ?? response.systemMessage ?? response.reason
  );
}

/**
 * Translate an opencode edit tool call into the payload of the handler
 * tree that understands it. Returns `undefined` for tools that edit
 * nothing.
 */
function toolInvocation(
  event: "PreToolUse" | "PostToolUse",
  tool: string,
  args: Record<string, unknown> | undefined,
  ids: { sessionID: string; callID: string },
  cwd: string,
): Invocation | undefined {
  const common = { session_id: ids.sessionID, cwd, hook_event_name: event };
  const toolResponse = event === "PostToolUse" ? { tool_response: {} } : {};
  if ((tool === "write" || tool === "edit") && typeof args?.filePath === "string") {
    // opencode passes absolute paths; relative ones keep the reports short.
    const path = isAbsolute(args.filePath) ? relative(cwd, args.filePath) : args.filePath;
    return {
      tree: "hook",
      payload: {
        ...common,
        ...toolResponse,
        transcript_path: "",
        tool_name: tool === "write" ? "Write" : "Edit",
        tool_input: { file_path: path.startsWith("..") ? args.filePath : path },
      },
    };
  }
  if (tool === "apply_patch" && typeof args?.patchText === "string") {
    // opencode's `apply_patch` speaks Codex's patch envelope, so the Codex
    // handlers parse the touched paths.
    return {
      tree: "codex-hook",
      payload: {
        ...common,
        ...toolResponse,
        model: "opencode",
        turn_id: ids.callID,
        tool_use_id: ids.callID,
        tool_name: "apply_patch",
        tool_input: { command: args.patchText },
      },
    };
  }
  return undefined;
}

async function AgentLensPlugin({ directory, client }: PluginInput) {
  const run = spawnRun(process.env.AGENT_LENS_BIN || "agent-lens", directory);
  const skip = parseSkip(process.env.AGENT_LENS_OPENCODE_SKIP);
  const enabled = (id: HookId) => !skip.has(id) && !skip.has(id.split(":")[0]);

  /** Run one handler; `undefined` when it is disabled, silent, or failed. */
  async function invoke(id: HookId, inv: Invocation): Promise<HookResponse | undefined> {
    if (!enabled(id)) return undefined;
    const [event, name] = id.split(":");
    try {
      const out = await run([inv.tree, event, name], JSON.stringify(inv.payload));
      return out.trim() ? (JSON.parse(out) as HookResponse) : undefined;
    } catch (err) {
      console.error(`agent-lens ${id} failed:`, err);
      return undefined;
    }
  }

  async function reports(ids: HookId[], inv: Invocation): Promise<string[]> {
    const responses = await Promise.all(ids.map((id) => invoke(id, inv)));
    return responses.map((r) => r && messageOf(r)).filter((m): m is string => !!m);
  }

  const childSessions = new Set<string>();
  /** Per root session: the summary to inject, once session start has run. */
  const sessionStarts = new Map<string, Promise<string | undefined>>();
  /** Pre-edit reports waiting for their tool call to finish, by call id. */
  const pendingPre = new Map<string, string[]>();
  /** Root sessions whose last stop was turned into a follow-up prompt. */
  const continued = new Set<string>();

  function sessionStart(sessionID: string): Promise<string | undefined> {
    let started = sessionStarts.get(sessionID);
    if (!started) {
      const inv: Invocation = {
        tree: "hook",
        payload: {
          hook_event_name: "SessionStart",
          session_id: sessionID,
          transcript_path: "",
          cwd: directory,
          source: "startup",
        },
      };
      // The snapshot must land before the first edit; the summary is only
      // read when the first system prompt is built.
      started = invoke("session-start:snapshot", inv).then(async () => {
        const summary = await invoke("session-start:summary", inv);
        return summary && messageOf(summary);
      });
      sessionStarts.set(sessionID, started);
    }
    return started;
  }

  async function stop(sessionID: string) {
    const stopHookActive = continued.delete(sessionID);
    const response = await invoke("stop:delta", {
      tree: "hook",
      payload: {
        hook_event_name: "Stop",
        session_id: sessionID,
        transcript_path: "",
        cwd: directory,
        stop_hook_active: stopHookActive,
      },
    });
    if (!response) return;
    try {
      if (response.decision === "block" && response.reason) {
        // opencode cannot veto going idle, so a blocking stop resumes the
        // session with the regression report as the next prompt.
        continued.add(sessionID);
        await client.session.promptAsync({
          path: { id: sessionID },
          body: { parts: [{ type: "text", text: response.reason }] },
        });
      } else if (response.systemMessage) {
        await client.tui.showToast({
          body: { title: "agent-lens", message: response.systemMessage, variant: "warning" },
        });
      }
    } catch (err) {
      console.error("agent-lens stop:delta could not report:", err);
    }
  }

  return {
    event: async ({ event }: { event: OpencodeEvent }) => {
      const { info, sessionID } = event.properties;
      if (event.type === "session.created" && info) {
        if (info.parentID) childSessions.add(info.id);
        else void sessionStart(info.id);
      } else if (event.type === "session.idle" && sessionID && !childSessions.has(sessionID)) {
        await stop(sessionID);
      }
    },

    "experimental.chat.system.transform": async (
      input: { sessionID?: string },
      output: { system: string[] },
    ) => {
      if (!input.sessionID || childSessions.has(input.sessionID)) return;
      const summary = await sessionStart(input.sessionID);
      if (summary) output.system.push(summary);
    },

    "tool.execute.before": async (
      input: { tool: string; sessionID: string; callID: string },
      output: { args: Record<string, unknown> },
    ) => {
      const inv = toolInvocation("PreToolUse", input.tool, output.args, input, directory);
      if (inv) pendingPre.set(input.callID, await reports(PRE_HOOKS, inv));
    },

    "tool.execute.after": async (
      input: { tool: string; sessionID: string; callID: string; args: Record<string, unknown> },
      output: { output: string },
    ) => {
      const pre = pendingPre.get(input.callID);
      pendingPre.delete(input.callID);
      const inv = toolInvocation("PostToolUse", input.tool, input.args, input, directory);
      const messages = [...(pre ?? []), ...(inv ? await reports(POST_HOOKS, inv) : [])];
      if (messages.length > 0) output.output = [output.output, ...messages].join("\n\n");
    },
  };
}

export default { id: "agent-lens", server: AgentLensPlugin };

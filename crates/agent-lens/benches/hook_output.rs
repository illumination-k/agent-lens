//! How much context each hook handler injects, in bytes and estimated
//! tokens, on a fixed corpus.
//!
//! Hook output is spent on every turn of every session, so its size is a
//! cost the timing benchmarks cannot see: a handler can get faster while
//! the text it hands the agent doubles. This target measures no time. It
//! drives the built binary through one session — snapshot, summary,
//! pre-edit checks, an edit, post-edit checks, stop — and prints one row
//! per handler, so a change in what a hook says shows up next to the
//! change that caused it (`mise run bench`, and both sides of
//! `mise run bench-compare`, print the table).

mod support;

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Bytes per token, the usual rough conversion for English-and-code
/// text. An estimate on purpose: the table is for seeing growth, and a
/// real tokenizer would tie the number to one model.
const BYTES_PER_TOKEN: usize = 4;

/// The file the session edits: a module with an `impl`, so every
/// per-file handler has something to say about it.
const EDITED_FILE: &str = "src/model_00.rs";

fn main() {
    // Cargo hands every bench target the harness flags (`--bench`,
    // `--save-baseline …`); only a request to list benchmarks changes
    // what this one does, and it has none to list.
    if std::env::args().any(|arg| arg == "--list") {
        return;
    }
    let dir = support::corpus(|root| {
        support::write_rust_crate_corpus(root, 16, 32)?;
        support::write_git_history(root, 24)
    });
    let root = dir.path();
    let mut rows: Vec<(String, usize)> = Vec::new();
    let mut measure = |handler: &[&str], event: &str, tool: bool| {
        rows.push((handler.join(" "), context_bytes(root, handler, event, tool)));
    };

    measure(&["session-start", "snapshot"], "SessionStart", false);
    measure(&["session-start", "summary"], "SessionStart", false);
    measure(&["pre-tool-use", "complexity"], "PreToolUse", true);
    measure(&["pre-tool-use", "cohesion"], "PreToolUse", true);
    // The edit the post-edit handlers and the stop gate react to: a
    // branchy function, a near-copy of it, and a forwarding wrapper.
    append(
        &root.join(EDITED_FILE),
        "\npub fn pending_a(input: i64) -> i64 {\n    if input > 3 { input - 1 } else { input + 1 }\n}\n\
         \npub fn pending_b(value: i64) -> i64 {\n    if value > 3 { value - 1 } else { value + 1 }\n}\n\
         \npub fn pending_c(input: i64) -> i64 {\n    pending_a(input)\n}\n",
    );
    measure(&["post-tool-use", "similarity"], "PostToolUse", true);
    measure(&["post-tool-use", "wrapper"], "PostToolUse", true);
    measure(&["post-tool-use", "footprint"], "PostToolUse", true);
    measure(&["stop", "delta"], "Stop", false);

    let mut table = format!(
        "\nhook_output: context injected per handler ({BYTES_PER_TOKEN} bytes/token estimate)\n\n\
         | handler | bytes | est. tokens |\n| --- | ---: | ---: |\n"
    );
    let total: usize = rows.iter().map(|(_, bytes)| bytes).sum();
    for (handler, bytes) in rows
        .iter()
        .map(|(h, b)| (h.as_str(), *b))
        .chain([("total", total)])
    {
        let _ = writeln!(
            table,
            "| {handler} | {bytes} | {} |",
            bytes.div_ceil(BYTES_PER_TOKEN)
        );
    }
    std::io::stdout()
        .write_all(table.as_bytes())
        .unwrap_or_else(|err| panic!("failed to print the table: {err}"));
}

/// Run one handler on its event payload and return the bytes of text it
/// hands back: every string in its JSON reply. The reply's keys and
/// punctuation are protocol, not context.
fn context_bytes(root: &Path, handler: &[&str], event: &str, tool: bool) -> usize {
    let cwd = root.to_string_lossy();
    let mut payload = serde_json::json!({
        "session_id": "bench",
        "transcript_path": "/tmp/transcript",
        "cwd": cwd,
        "hook_event_name": event,
        "source": "startup",
    });
    if tool {
        payload["tool_name"] = "Edit".into();
        payload["tool_input"] = serde_json::json!({
            "file_path": root.join(EDITED_FILE).to_string_lossy(),
        });
        payload["tool_response"] = serde_json::json!({});
    }
    let mut child = Command::new(env!("CARGO_BIN_EXE_agent-lens"))
        .arg("hook")
        .args(handler)
        .current_dir(root)
        .env("RUST_LOG", "off")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|err| panic!("failed to spawn agent-lens: {err}"));
    child
        .stdin
        .take()
        .unwrap_or_else(|| panic!("no stdin for agent-lens hook"))
        .write_all(payload.to_string().as_bytes())
        .unwrap_or_else(|err| panic!("failed to write the hook payload: {err}"));
    let output = child
        .wait_with_output()
        .unwrap_or_else(|err| panic!("agent-lens hook did not finish: {err}"));
    let name = handler.join(" ");
    assert!(
        output.status.success(),
        "agent-lens hook {name} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|err| panic!("agent-lens hook {name} did not print JSON: {err}"));
    string_bytes(&reply)
}

/// Total length of every string value in `value`.
fn string_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(items) => items.iter().map(string_bytes).sum(),
        serde_json::Value::Object(fields) => fields.values().map(string_bytes).sum(),
        _ => 0,
    }
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    file.write_all(text.as_bytes())
        .unwrap_or_else(|err| panic!("failed to edit {}: {err}", path.display()));
}

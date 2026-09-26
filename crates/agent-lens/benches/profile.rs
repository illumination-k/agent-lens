//! End-to-end `agent-lens run <profile>` over a git repository, driven
//! through the built binary.
//!
//! This is the surface the analyzer benchmarks cannot see: the analysis
//! index shared across a profile's tools, the batched `git diff` behind
//! `--diff-only`, the `git log` walk behind churn, and CLI start-up. Each
//! of those has regressed a profile run while every per-analyzer
//! benchmark stayed flat.

mod support;

use std::path::Path;
use std::process::Command;

use criterion::{Criterion, criterion_group, criterion_main};

/// Mirrors the repository's own `self` and `changes` profiles.
const CONFIG: &str = r#"
[profile.full]
path = "."
format = "json"
tools = [
  "complexity", "cohesion", "similarity", "forwarding",
  "coupling", "communities", "context-span", "cycles", "layers", "hubs",
  "hotspot", "risk",
]

[profile.history]
path = "."
format = "json"
tools = ["hotspot", "risk", "co-change", "change-entropy"]

[profile.changes]
path = "."
format = "json"
tools = ["similarity", "forwarding", "complexity", "cohesion", "change-entropy", "footprint"]

[profile.changes.similarity]
diff-only = true
[profile.changes.complexity]
diff-only = true
[profile.changes.cohesion]
diff-only = true
[profile.changes.forwarding]
diff-only = true
[profile.changes.change-entropy]
diff-only = true
[profile.changes.footprint]
diff-only = true
"#;

fn bench_profile(c: &mut Criterion) {
    let dir = support::corpus(|root| {
        support::write_rust_crate_corpus(root, 16, 32)?;
        std::fs::write(root.join("agent-lens.toml"), CONFIG)?;
        support::write_git_history(root, 24)?;
        support::write_pending_edit(root)
    });
    for profile in ["full", "history", "changes"] {
        c.bench_function(&format!("profile_run_{profile}"), |b| {
            b.iter(|| run_profile(dir.path(), profile));
        });
    }
}

fn run_profile(root: &Path, profile: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_agent-lens"))
        .args(["run", profile])
        .current_dir(root)
        .env("RUST_LOG", "off")
        .output()
        .unwrap_or_else(|err| panic!("failed to spawn agent-lens: {err}"));
    assert!(
        output.status.success(),
        "agent-lens run {profile} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::hint::black_box(output.stdout.len());
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(10);
    targets = bench_profile
}
criterion_main!(benches);

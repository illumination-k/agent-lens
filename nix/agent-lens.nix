{
  lib,
  rustPlatform,
  git,
}:

rustPlatform.buildRustPackage {
  pname = "agent-lens";
  version = "0.1.0";

  # Only the inputs the workspace actually compiles from. `web/`, `docs/`,
  # and `target/` are deliberately excluded so editing them does not
  # invalidate the build. `.claude/skills` is required: `skills.rs` embeds
  # every `SKILL.md` with `include_str!`.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../crates
      ../.claude/skills
    ];
  };

  cargoLock = {
    lockFile = ../Cargo.lock;
    # `lens-py` depends on unpublished ruff crates pulled straight from git.
    # One entry covers the whole checkout: `ruff_python_parser`,
    # `ruff_text_size`, and the crates they pull in share this source.
    # Update it whenever the ruff tag in `crates/lens-py/Cargo.toml` moves.
    outputHashes = {
      "ruff_python_ast-0.0.0" = "sha256-DH00tENXdCdNcGPXPGzZsU3RVYQ0VBe1QLvbgEg/G6k=";
    };
  };

  # Several analyzer tests shell out to `git` to build throwaway repositories.
  nativeCheckInputs = [ git ];

  # `buildRustPackage` exports `RUST_LOG=""` by default, and an empty
  # `RUST_LOG` makes `EnvFilter` drop every event — including the
  # `agent-lens failed` errors that `tests/cli_smoke.rs` asserts on.
  logLevel = "info";

  meta = {
    description = "Single-binary CLI giving coding agents a lens for seeing codebases more deeply";
    homepage = "https://github.com/illumination-k/agent-lens";
    license = lib.licenses.mit;
    mainProgram = "agent-lens";
    platforms = lib.platforms.unix;
  };
}

{
  mkShell,
  rustPlatform,
  rustc,
  cargo,
  clippy,
  rustfmt,
  rust-analyzer,
  cargo-nextest,
  cargo-deny,
  cargo-audit,
  cargo-mutants,
  cargo-llvm-cov,
  dprint,
  shfmt,
  shellcheck,
  actionlint,
  zizmor,
  nodejs,
  pnpm,
  uv,
  git,
}:

# Mirrors the toolchain `mise.*.toml` installs, but from nixpkgs so it works
# on NixOS and other hosts where mise's pre-built binaries cannot run.
# Versions track nixpkgs rather than the mise pins, so CI stays the source of
# truth for exact versions.
mkShell {
  name = "agent-lens";

  packages = [
    rustc
    cargo
    clippy
    rustfmt
    rust-analyzer
    cargo-nextest
    cargo-deny
    cargo-audit
    cargo-mutants
    cargo-llvm-cov
    dprint
    shfmt
    shellcheck
    actionlint
    zizmor
    nodejs
    pnpm
    uv
    git
  ];

  # rust-analyzer and `cargo doc --open` need the std sources to resolve
  # anything from `core`/`std`.
  RUST_SRC_PATH = "${rustPlatform.rustLibSrc}";
}

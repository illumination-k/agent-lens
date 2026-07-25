{
  description = "agent-lens: a lens for coding agents to see codebases more deeply";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      # `nix build`, `nix run github:illumination-k/agent-lens`,
      # `nix profile install github:illumination-k/agent-lens`
      packages = forAllSystems (pkgs: rec {
        agent-lens = pkgs.callPackage ./nix/agent-lens.nix { };
        default = agent-lens;
      });

      # `nix develop` — the toolchain `mise install` would otherwise provide.
      devShells = forAllSystems (pkgs: {
        default = pkgs.callPackage ./nix/devshell.nix { };
      });

      # `nix flake check`
      checks = forAllSystems (pkgs: {
        agent-lens = self.packages.${pkgs.stdenv.hostPlatform.system}.agent-lens;
      });

      # `nix fmt`
      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);

      # For downstream flakes: `inputs.agent-lens.overlays.default`.
      overlays.default = final: _prev: {
        agent-lens = final.callPackage ./nix/agent-lens.nix { };
      };
    };
}

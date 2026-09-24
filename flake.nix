{
  description = "agent-sessions: one dashboard for every agentic session and worktree";

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
      packages = forAllSystems (pkgs: rec {
        agent-sessions = pkgs.callPackage ./nix/package.nix { };
        default = agent-sessions;
      });

      checks = forAllSystems (pkgs: {
        agent-sessions = self.packages.${pkgs.system}.agent-sessions;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.cargo-llvm-cov
            pkgs.llvmPackages.llvm
            # The coverage gate reads llvm-cov's exported segments.
            pkgs.jq
            pkgs.tmux
            pkgs.git
            pkgs.just
            pkgs.shellcheck
          ];
          LLVM_COV = pkgs.lib.getExe' pkgs.llvmPackages.llvm "llvm-cov";
          LLVM_PROFDATA = pkgs.lib.getExe' pkgs.llvmPackages.llvm "llvm-profdata";
        };
      });
    };
}

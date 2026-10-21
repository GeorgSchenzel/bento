{
  # Embedded into the bento binary and built by the builder VM into the GC
  # root /nix/var/nix/gcroots/bento/tools; every box gets its bin/ on PATH.
  description = "bento box tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  # opencode v2 isn't released yet; build the branch with its own flake. No
  # nixpkgs follows: its node_modules hashes are pinned to its own bun.
  inputs.opencode.url = "github:anomalyco/opencode/v2";

  outputs =
    { nixpkgs, opencode, ... }:
    let
      systems = [
        "aarch64-linux"
        "x86_64-linux"
      ];
    in
    {
      packages = nixpkgs.lib.genAttrs systems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfree = true; # claude-code
          };
        in
        {
          default = pkgs.buildEnv {
            name = "bento-tools";
            paths = with pkgs; [
              claude-code
              opencode.packages.${system}.opencode
              devenv # boxes run in the checkout's devenv shell
              git
              socat
              ncurses # terminfo for the host's TERM
            ];
          };
        }
      );
    };
}

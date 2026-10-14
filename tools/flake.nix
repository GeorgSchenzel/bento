{
  # Embedded into the bento binary and built by the builder VM into the GC
  # root /nix/var/nix/gcroots/bento/tools; every box gets its bin/ on PATH.
  description = "bento box tools";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
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
              opencode
              git
              socat
              ncurses # terminfo for the host's TERM
            ];
          };
        }
      );
    };
}

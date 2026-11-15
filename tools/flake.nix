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
          pkgs = import nixpkgs { inherit system; };
          # every `bento exec` runs its command through this (bento-run.c)
          bento-run = pkgs.runCommandCC "bento-run" { } ''
            mkdir -p $out/bin
            $CC -O2 -Wall -o $out/bin/bento-run ${./bento-run.c}
          '';
        in
        {
          default = pkgs.buildEnv {
            name = "bento-tools";
            # agents and other extra tools come from the config's `tools`
            paths = with pkgs; [
              bento-run
              bashInteractive # bento's scripts and `enter`
              coreutils
              findutils
              gnugrep
              gnused
              gnutar # cp and copy
              gzip
              procps
              util-linux # mount, findmnt, setsid for the boot script
              cacert # git and nix over https
              devenv
              docker # dockerd runs in every box; includes compose and buildx
              git
              just
              python3
              uv
              socat # channel shims and nix forwarding
              ncurses # terminfo for the host's TERM
            ];
          };
        }
      );
    };
}

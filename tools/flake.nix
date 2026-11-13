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

      # herdr-core (hc) runs the agents (decisions §4). Static release
      # tarballs from herdr's release server (linux builds are static-pie,
      # so no patchelf); sha256s from the release manifest
      # releases.herdr.dev/core/<version>/manifest.json; the tarballs were
      # downloaded and sha256-checked by hand (base16 below, for reading): x86_64
      # 78424755c10a40dde38a0bdb55c20ac19706e9ae05ed68fc2acf1fcff0d941c1,
      # aarch64 e844a2bfab48a226097679d076bc83042fe57fccef5387ef802e5b24e78b529a.
      herdrVersion = "0.1.21";
      herdrAssets = {
        x86_64-linux.hash = "sha256-eEJHVcEKQN3jigvbVcIKwZcG6a4F7Wj8Ks8fz/DZQcE=";
        aarch64-linux.hash = "sha256-6ESiv6tIoiYJdnnQdryDBC/lf8zvU4fvgC5bJOeLUpo=";
      };
      # The asset name is herdr-core-<os>-<cpu> (e.g. linux-x86_64), while
      # nix system names are <cpu>-<os>.
      herdrUrl =
        system:
        let
          cpu = builtins.head (nixpkgs.lib.splitString "-" system);
        in
        "https://releases.herdr.dev/core/${herdrVersion}/herdr-core-linux-${cpu}.tar.gz";
    in
    {
      packages = nixpkgs.lib.genAttrs systems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            config.allowUnfree = true; # claude-code
          };
          lib = nixpkgs.lib;

          herdr-core = pkgs.stdenvNoCC.mkDerivation {
            pname = "herdr-core";
            version = herdrVersion;
            src = pkgs.fetchurl {
              url = herdrUrl system;
              inherit (herdrAssets.${system}) hash;
            };
            sourceRoot = "."; # flat tarball: herdr-core, LICENSE, notices
            dontConfigure = true;
            dontBuild = true;
            dontFixup = true; # prebuilt, stripped, static: nothing to fix
            installPhase = ''
              runHook preInstall
              install -Dm 0755 herdr-core $out/bin/herdr-core
              ln -s herdr-core $out/bin/hc
              install -Dm 0644 LICENSE THIRD-PARTY-NOTICES -t $out/share/doc/herdr-core/
              runHook postInstall
            '';
          };

          # The box-side forest client (decisions §2): this repo's python
          # script with a store-path python3 shebang, so it runs regardless
          # of the box's PATH.
          forest = pkgs.writeScriptBin "forest" (
            let
              body = builtins.readFile ./forest-client.py;
              rest = lib.concatStringsSep "\n" (lib.tail (lib.splitString "\n" body));
            in
            # the shebang line is dropped and replaced; make sure it is one
            assert lib.head (lib.splitString "\n" body) == "#!/usr/bin/env python3";
            "#!${pkgs.python3}/bin/python3\n${rest}"
          );
        in
        {
          default = pkgs.buildEnv {
            name = "bento-tools";
            paths =
              with pkgs;
              [
                claude-code
                opencode
                devenv # boxes run in the checkout's devenv shell
                docker # dockerd runs in every box; includes compose and buildx
                git
                just # the agents' guide points at justfiles
                python3 # agents shell out to python3
                uv # python projects and tools
                socat
                ncurses # terminfo for the host's TERM
              ]
              ++ [ herdr-core forest ];
          };
        }
      );
    };
}

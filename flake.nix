{
  description = "bento — per-checkout microsandbox boxes on a shared nix store";

  inputs = {
    # Used to build the bento package.
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

    # Used for devShells only; mirrors the inputs in devenv.yaml so that
    # `nix develop` and `devenv shell` produce the same environment.
    devenv-nixpkgs.url = "github:cachix/devenv-nixpkgs/rolling";

    devenv = {
      url = "github:cachix/devenv";
      inputs.nixpkgs.follows = "devenv-nixpkgs";
    };

    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "devenv-nixpkgs";
    };
  };

  nixConfig = {
    extra-trusted-public-keys = "devenv.cachix.org-1:w1cLUi8dv3hnoSPGAuibQv+f9TZLr6cv/Hm9XgU50cw=";
    extra-substituters = "https://devenv.cachix.org";
  };

  outputs =
    inputs@{
      self,
      nixpkgs,
      devenv,
      ...
    }:
    let
      # macOS only for now: the shared store is an APFS sparse image
      # (hdiutil), and microsandbox needs Apple Silicon there
      packageSystems = [ "aarch64-darwin" ];
      # other systems keep a devShell for development
      shellSystems = packageSystems ++ [
        "x86_64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs packageSystems;

      bento =
        pkgs:
        let
          # The SDK's "prebuilt" feature has build scripts that download the
          # microsandbox runtime (msb + libkrunfw) and the guest agentd
          # binary from GitHub releases at build time — impossible in the nix
          # sandbox. Instead we stage them via fetchurl:
          #   - MSB_AGENTD_PATH embeds agentd (guest PID 1) into bento
          #   - MSB_HOME pre-seeded with the bundle satisfies the build
          #     script's already-installed guard, skipping its download.
          # Keep in sync with the microsandbox dependency in Cargo.toml:
          # bumping it requires new hashes below.
          msbVersion = "0.7.8";
          release = "https://github.com/superradcompany/microsandbox/releases/download/v${msbVersion}";

          target = {
            arch = "aarch64";
            os = "darwin";
            bundleHash = "sha256-Xb4Nkhxrb4B9LFn4fgo8OmSEJIcba+5RO4N6jGAKJJM=";
          };

          runtimeBundle = pkgs.fetchurl {
            url = "${release}/microsandbox-${target.os}-${target.arch}.tar.gz";
            hash = target.bundleHash;
          };

          # agentd runs as PID 1 inside the (always Linux) guest
          agentd = pkgs.fetchurl {
            url = "${release}/agentd-${target.arch}";
            hash = "sha256-G/yU0eOXQIIBDNwWv534b5ZU1SrChO/ktDn7xnj0JvM=";
          };
        in
        pkgs.rustPlatform.buildRustPackage {
          pname = "bento";
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;

          src = pkgs.lib.cleanSourceWith {
            src = pkgs.lib.cleanSource ./.;
            filter =
              path: type:
              pkgs.lib.sources.cleanSourceFilter path type
              && !builtins.elem (baseNameOf path) [
                "target"
                ".devenv"
                ".venv"
              ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          # crate tests require the microsandbox runtime; behavior is covered
          # by the e2e suite
          doCheck = false;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          preBuild = ''
            export MSB_AGENTD_PATH=${agentd}

            export MSB_HOME=$(mktemp -d)
            mkdir -p $MSB_HOME/bin $MSB_HOME/lib
            # the bundle is flat: msb + libkrunfw; mirror the build script's
            # extract rules (libkrunfw* → lib/, rest → bin/)
            staging=$(mktemp -d)
            tar -xzf ${runtimeBundle} -C $staging
            for f in "$staging"/*; do
              case $(basename "$f") in
                libkrunfw*) cp -L "$f" $MSB_HOME/lib/ ;;
                *) cp "$f" $MSB_HOME/bin/ && chmod +x $MSB_HOME/bin/$(basename "$f") ;;
              esac
            done
          '';

          # git (repo discovery, in-box identity) is wrapped into PATH; the
          # microsandbox runtime is managed by `bento install`, and nix on the
          # host is not needed at runtime (it runs in the builder VM).
          postInstall = ''
            wrapProgram $out/bin/bento \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git ]}
          '';

          meta = {
            description = "Per-checkout microsandbox boxes on a shared nix store";
            mainProgram = "bento";
            platforms = packageSystems;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          bento = bento pkgs;
          default = self.packages.${system}.bento;
        }
      );

      devShells = nixpkgs.lib.genAttrs shellSystems (
        system:
        let
          pkgs = import inputs.devenv-nixpkgs { inherit system; };
        in
        {
          default = devenv.lib.mkShell {
            inherit inputs pkgs;
            modules = [ ./devenv.nix ];
          };
        }
      );

      overlays.default = final: _prev: { bento = bento final; };

      homeManagerModules.bento =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.bento;
          bentoPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.bento;
        in
        {
          options.programs.bento.enable = lib.mkEnableOption "bento";
          config = lib.mkIf cfg.enable {
            home.packages = [ bentoPkg ];
          };
        };
    };
}

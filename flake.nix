{
  description = "bento — run any repo's devenv inside a microsandbox box";

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

    nix2container = {
      url = "github:nlewo/nix2container";
      inputs.nixpkgs.follows = "devenv-nixpkgs";
    };

    # no nixpkgs follow here: mk-shell-bin declares no nixpkgs input
    mk-shell-bin.url = "github:rrbutani/nix-mk-shell-bin";
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
      # microsandbox is Linux-only, so the package is too
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;

      bento =
        pkgs:
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

          nativeBuildInputs = [ pkgs.makeWrapper ];

          # git is a stateless runtime dependency used for repo discovery, so
          # it is wrapped into PATH. Stateful tools (devenv, the microsandbox
          # runtime) are intentionally not wrapped: bento runs the target
          # repo's devenv against the user's nix, and the runtime is managed
          # by `bento install`.
          postInstall = ''
            wrapProgram $out/bin/bento \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.git ]}
          '';

          meta = {
            description = "Run any repo's devenv inside a microsandbox box";
            mainProgram = "bento";
            platforms = systems;
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

      devShells = nixpkgs.lib.genAttrs (systems ++ [
        "x86_64-darwin"
        "aarch64-darwin"
      ]) (
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

      nixosModules.bento =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.bento;
          bentoPkg = self.packages.${pkgs.system}.bento;
        in
        {
          options.programs.bento.enable = lib.mkEnableOption "bento";
          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ bentoPkg ];
          };
        };

      homeManagerModules.bento =
        {
          config,
          lib,
          pkgs,
          ...
        }:
        let
          cfg = config.programs.bento;
          bentoPkg = self.packages.${pkgs.system}.bento;
        in
        {
          options.programs.bento.enable = lib.mkEnableOption "bento";
          config = lib.mkIf cfg.enable {
            home.packages = [ bentoPkg ];
          };
        };
    };
}

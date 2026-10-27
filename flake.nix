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
      # macOS on Apple Silicon only for now: the shared store is an APFS
      # sparse image (hdiutil), and microsandbox needs Apple Silicon there
      packageSystems = [ "aarch64-darwin" ];
      # other systems keep a devShell for development
      shellSystems = packageSystems ++ [
        "x86_64-darwin"
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs packageSystems;
    in
    {
      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs { inherit system; };
        in
        {
          bento = pkgs.callPackage ./nix/package.nix { };
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

      overlays.default = final: _prev: { bento = final.callPackage ./nix/package.nix { }; };

      homeManagerModules.bento = import ./nix/home-manager.nix self packageSystems;
    };
}

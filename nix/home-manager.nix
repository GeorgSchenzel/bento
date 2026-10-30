# programs.bento on home-manager: the bento CLI on the user's PATH.
self: systems:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.bento;
  system = pkgs.stdenv.hostPlatform.system;
  supported = builtins.elem system systems;
in
{
  options.programs.bento = {
    enable = lib.mkEnableOption "bento";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.bento;
      defaultText = lib.literalExpression "bento.packages.\${system}.bento";
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = supported;
        message = "bento is only packaged for ${lib.concatStringsSep ", " systems}: microsandbox needs Apple Silicon on macOS.";
      }
    ];

    home.packages = lib.mkIf supported [ cfg.package ];
  };
}

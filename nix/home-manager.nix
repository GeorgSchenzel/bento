# programs.bento on home-manager: the bento CLI, its global config
# (~/.config/bento/config.yaml, contracts.md) and agent presets. The presets
# are this module's, not bento's: each expands into plain config keys (the
# agent as a tool, its settings with permission prompts off, the host's agent
# config read-only, its credentials).
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
  yaml = pkgs.formats.yaml { };
  json = pkgs.formats.json { };

  # pinned to the nixpkgs bento itself is built from
  nixpkgsRef = "github:NixOS/nixpkgs/${self.inputs.nixpkgs.rev}";

  claude = cfg.agents.claude;
  opencode = cfg.agents.opencode;

  # claude reads managed settings with the highest precedence, so the box's
  # permission mode never depends on the host's settings.json
  claudeManaged = json.generate "bento-claude-managed-settings.json" {
    permissions.defaultMode = "bypassPermissions";
    skipDangerousModePermissionPrompt = true;
  };
  # claude's first-run onboarding, skipped (auth comes from the token)
  claudeState = json.generate "bento-claude.json" { hasCompletedOnboarding = true; };

  opencodeBox = json.generate "bento-opencode.json" {
    "$schema" = "https://opencode.ai/config.json";
    permission = {
      edit = "allow";
      bash = "allow";
      webfetch = "allow";
    };
  };

  hostEntries =
    dir: names:
    map (n: {
      guest = "~/${dir}/${n}";
      host = "~/${dir}/${n}";
    }) names;
in
{
  options.programs.bento = {
    enable = lib.mkEnableOption "bento";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${system}.bento;
      defaultText = lib.literalExpression "bento.packages.\${system}.bento";
    };

    settings = lib.mkOption {
      type = yaml.type;
      default = { };
      description = ''
        bento's global config (contracts.md, Global config), written to
        ~/.config/bento/config.yaml. Changes apply at each box's next boot.
      '';
      example = lib.literalExpression ''
        {
          channels.wok.socket = "~/.local/share/wok/wok.sock";
          git."github.com" = "~/.config/bento/secrets/github";
        }
      '';
    };

    agents.claude = {
      enable = lib.mkEnableOption "claude in every box (a preset)";
      tool = lib.mkOption {
        type = lib.types.str;
        default = "${nixpkgsRef}#claude-code";
        description = "Flake ref of claude, built by the builder.";
      };
      tokenFile = lib.mkOption {
        type = lib.types.str;
        default = "~/.config/bento/secrets/claude";
        description = "A `claude setup-token` token, as CLAUDE_CODE_OAUTH_TOKEN (read at each exec).";
      };
      hostConfig = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [
          "settings.json"
          "plugins"
        ];
        description = ''
          Entries of the host's ~/.claude the boxes get read-only. Each must
          exist (a missing one fails the boot). Not `skills` by default: stew's
          module mounts its own there.
        '';
      };
    };

    agents.opencode = {
      enable = lib.mkEnableOption "opencode in every box (a preset)";
      tool = lib.mkOption {
        type = lib.types.str;
        default = "${nixpkgsRef}#opencode";
      };
      authFile = lib.mkOption {
        type = lib.types.str;
        default = "~/.local/share/opencode/auth.json";
        description = "Copied into every box at each boot (opencode rewrites it).";
      };
      hostConfig = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ "opencode.json" ];
        description = "Entries of the host's ~/.config/opencode the boxes get read-only.";
      };
    };
  };

  config = lib.mkIf cfg.enable (
    lib.mkMerge [
      {
        assertions = [
          {
            assertion = supported;
            message = "bento is only packaged for ${lib.concatStringsSep ", " systems}.";
          }
        ];
        home.packages = lib.mkIf supported [ cfg.package ];
        xdg.configFile."bento/config.yaml".source = yaml.generate "bento-config.yaml" cfg.settings;
      }
      (lib.mkIf claude.enable {
        programs.bento.settings = {
          tools.claude = claude.tool;
          files = [
            {
              guest = "/etc/claude-code/managed-settings.json";
              host = "${claudeManaged}";
            }
          ]
          ++ hostEntries ".claude" claude.hostConfig;
          copy = [
            {
              guest = "~/.claude/.claude.json";
              host = "${claudeState}";
            }
          ];
          env = {
            CLAUDE_CONFIG_DIR = "${config.home.homeDirectory}/.claude";
            # the "trust this folder" prompt, which the permission mode doesn't cover
            CLAUDE_CODE_SANDBOXED = "1";
            CLAUDE_CODE_NO_FLICKER = "1";
          };
          env_files.CLAUDE_CODE_OAUTH_TOKEN = claude.tokenFile;
        };
      })
      (lib.mkIf opencode.enable {
        programs.bento.settings = {
          tools.opencode = opencode.tool;
          files = [
            {
              guest = "/etc/opencode-bento/opencode.json";
              host = "${opencodeBox}";
            }
          ]
          ++ hostEntries ".config/opencode" opencode.hostConfig;
          copy = [
            {
              guest = "~/.local/share/opencode/auth.json";
              host = opencode.authFile;
            }
          ];
          env.OPENCODE_CONFIG = "/etc/opencode-bento/opencode.json";
        };
      })
    ]
  );
}

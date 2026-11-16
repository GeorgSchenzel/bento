# programs.bento on home-manager: the bento CLI, its global config
# (~/.config/bento/config.yaml, contracts.md) and agent presets. The presets
# are this module's, not bento's: each expands into plain config keys (the
# agent as a tool, its settings generated here with permission prompts off, its
# credentials from secret files at each exec). Nothing of the host's own agent
# config goes in: two hosts with the same nix config make the same boxes.
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

  # `zai-coding-plan` → `ZAI_CODING_PLAN`, for env names
  envName = n: lib.toUpper (lib.replaceStrings [ "-" "." ] [ "_" "_" ] n);
  claudeTokenVar = n: "CLAUDE_ACCOUNT_${envName n}_TOKEN";
  opencodeKeyVar = n: "OPENCODE_KEY_${envName n}";

  # the person's settings, then the box's: keys from the env, no permission prompts
  opencodeBox = json.generate "bento-opencode.json" (
    lib.recursiveUpdate (removeAttrs opencode.settings [ "permission" ]) {
      "$schema" = "https://opencode.ai/config.json";
      permission."*" = "allow";
      provider = lib.mapAttrs (n: _: {
        options.apiKey = "{env:${opencodeKeyVar n}}";
      }) opencode.apiKeyFiles;
    }
  );
in
{
  imports = [
    (lib.mkRemovedOptionModule [ "programs" "bento" "agents" "claude" "tokenFile" ]
      "Use programs.bento.agents.claude.accounts.<name> = <token file>."
    )
    (lib.mkRemovedOptionModule [ "programs" "bento" "agents" "claude" "hostConfig" ]
      "Boxes get no host agent config; set claude's model with agents.claude.model."
    )
    (lib.mkRemovedOptionModule [ "programs" "bento" "agents" "opencode" "authFile" ]
      "opencode 2.x ignores a copied auth.json; use agents.opencode.apiKeyFiles.<provider> = <key file>."
    )
    (lib.mkRemovedOptionModule [ "programs" "bento" "agents" "opencode" "hostConfig" ]
      "Boxes get no host agent config; use agents.opencode.settings (e.g. config.programs.opencode.settings)."
    )
  ];

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
        }
      '';
    };

    git = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = ''
        A token file per git host (the global config's `git`), for https
        clones and pushes in every box. Paths outside the nix store (e.g.
        sops-nix's), `~` is HOME; only the path goes into the config. A missing
        file is a warning at boot: that host gets no credentials.
      '';
      example = lib.literalExpression ''
        {
          "github.com" = "~/.config/sops-nix/secrets/git/github-com";
          "gitlab.com" = "~/.config/sops-nix/secrets/git/gitlab-com";
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
      accounts = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        example = lib.literalExpression ''
          {
            main = config.sops.secrets."bento/claude-token".path;
            second = config.sops.secrets."bento/claude-token-2".path;
          }
        '';
        description = ''
          claude accounts by name: the path of a file holding each one's
          `claude setup-token` token (read on the host at each exec; only the
          path goes into the config; an unreadable one fails every exec, enter
          and cp, 125). Every exec gets each as CLAUDE_ACCOUNT_<NAME>_TOKEN, and
          the default one as CLAUDE_CODE_OAUTH_TOKEN: a launcher picks another
          by setting CLAUDE_CODE_OAUTH_TOKEN from its variable (stew's
          `--account`).
        '';
      };
      defaultAccount = lib.mkOption {
        type = lib.types.str;
        default = if claude.accounts == { } then "" else lib.head (lib.attrNames claude.accounts);
        defaultText = "the first account by name";
        description = "The account claude uses unless a launcher picks another.";
      };
      model = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "opus";
        description = ''
          claude's model in boxes (ANTHROPIC_MODEL; `/model` still changes it
          for a session). With a token alone claude doesn't know the account's
          plan and starts on its plain default, so set it.
        '';
      };
    };

    agents.opencode = {
      enable = lib.mkEnableOption "opencode in every box (a preset)";
      tool = lib.mkOption {
        type = lib.types.str;
        default = "${nixpkgsRef}#opencode";
      };
      apiKeyFiles = lib.mkOption {
        type = lib.types.attrsOf lib.types.str;
        default = { };
        example = lib.literalExpression ''
          { zai-coding-plan = config.sops.secrets.zai-api-key.path; }
        '';
        description = ''
          A file holding the API key per opencode provider id. Every exec gets
          each as OPENCODE_KEY_<ID>, and the box's opencode.json sets
          `provider.<id>.options.apiKey` to it: nothing is stored in the box
          (opencode 2.x keeps logins in its database and reads auth.json only
          once, so a copied auth.json logs nothing in).
        '';
      };
      settings = lib.mkOption {
        type = json.type;
        default = { };
        example = lib.literalExpression "config.programs.opencode.settings";
        description = ''
          opencode.json for boxes (model, providers, ...). Its `permission` is
          replaced: boxes allow everything.
        '';
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
          {
            assertion = !claude.enable || claude.accounts ? ${claude.defaultAccount};
            message = "programs.bento.agents.claude: defaultAccount must name one of accounts (set at least one).";
          }
        ];
        home.packages = lib.mkIf supported [ cfg.package ];
        xdg.configFile."bento/config.yaml".source = yaml.generate "bento-config.yaml" cfg.settings;
      }
      (lib.mkIf (cfg.git != { }) { programs.bento.settings.git = cfg.git; })
      (lib.mkIf claude.enable {
        programs.bento.settings = {
          tools.claude = claude.tool;
          files = [
            {
              guest = "/etc/claude-code/managed-settings.json";
              host = "${claudeManaged}";
            }
          ];
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
          }
          // lib.optionalAttrs (claude.model != null) { ANTHROPIC_MODEL = claude.model; };
          # (no default account is an assertion, not an eval error here)
          env_files =
            lib.optionalAttrs (claude.accounts ? ${claude.defaultAccount}) {
              CLAUDE_CODE_OAUTH_TOKEN = claude.accounts.${claude.defaultAccount};
            }
            // lib.mapAttrs' (n: f: lib.nameValuePair (claudeTokenVar n) f) claude.accounts;
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
          ];
          env.OPENCODE_CONFIG = "/etc/opencode-bento/opencode.json";
          env_files = lib.mapAttrs' (n: f: lib.nameValuePair (opencodeKeyVar n) f) opencode.apiKeyFiles;
        };
      })
    ]
  );
}

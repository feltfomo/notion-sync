# home-manager module for notion-sync.
#
# Self-contained: defines the user service AND materializes config in the user's
# home, so a home-manager-only user can deploy notion-sync entirely in Nix.
#
#   imports = [ inputs.notion-sync.homeManagerModules.default ];
#   services.notion-sync = {
#     enable = true;
#     environmentFile = config.sops.secrets."notion-token".path; # 0600, NOTION_TOKEN=...
#     settings.mapping = [
#       { local_root = "/home/alice/projects/app";
#         parent_page_id = "0123...";
#         ignore = [ ".git" "target" ]; }
#     ];
#     perDirectory."projects/app/vendor".ignore = [ "*.generated" ];
#   };

self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.notion-sync;
  helpers = import ./lib.nix { inherit lib pkgs; };

  configPath =
    if cfg.configFile != null then
      toString cfg.configFile
    else
      "${config.xdg.configHome}/notion-sync/config.toml";
in
{
  options.services.notion-sync = {
    enable = lib.mkEnableOption "the notion-sync two-way sync daemon (home-manager)";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "notion-sync.packages.\${system}.default";
      description = "The notion-sync package to run.";
    };

    settings = helpers.settingsOption;
    configFile = helpers.configFileOption;
    perDirectory = helpers.perDirectoryOption;

    environmentFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to a 0600 EnvironmentFile that defines NOTION_TOKEN=...
        MUST NOT be a nix store path (the store is world-readable).
      '';
    };
  } // helpers.logOptions;

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.configFile != null || cfg.settings != { };
        message = "services.notion-sync: set either `settings` (preferred) or `configFile`.";
      }
    ];

    # Central config -> ~/.config/notion-sync/config.toml (unless the user brings
    # their own configFile).
    xdg.configFile = lib.mkIf (cfg.configFile == null) {
      "notion-sync/config.toml".source =
        helpers.renderConfig "notion-sync-config.toml" cfg.settings;
    };

    # Per-directory overrides, dropped into each mapped tree (paths relative to $HOME).
    home.file = lib.mapAttrs'
      (dir: value:
        lib.nameValuePair "${dir}/.notion-sync.toml" {
          source = helpers.renderConfig "notion-sync-perdir.toml" value;
        })
      cfg.perDirectory;

    systemd.user.services.notion-sync = {
      Unit = {
        Description = "Two-way Notion <-> codebase sync daemon";
        Documentation = [ "https://github.com/feltfomo/notion-sync" ];
        After = [ "network-online.target" ];
        Wants = [ "network-online.target" ];
      };
      Install.WantedBy = [ "default.target" ];
      Service = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/notion-sync ${helpers.execArgs configPath cfg}";
        # NOTION_TOKEN lives here, outside the nix store.
        EnvironmentFile = cfg.environmentFile;
        Environment = [ "RUST_LOG=${cfg.logLevel}" ];
        Restart = "on-failure";
        RestartSec = 5;
        # SIGTERM triggers graceful shutdown; give it room to drain.
        KillSignal = "SIGTERM";
        TimeoutStopSec = 30;
      };
    };
  };
}

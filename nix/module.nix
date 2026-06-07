# systemd *user* service module for notion-sync.
#
# Usage (NixOS, system-level user service):
#
#   imports = [ inputs.notion-sync.nixosModules.default ];
#   services.notion-sync = {
#     enable = true;
#     user = "alice";
#     configFile = "/home/alice/.config/notion-sync/config.toml";
#     environmentFile = "/home/alice/.config/notion-sync/token.env"; # 0600, contains NOTION_TOKEN=...
#   };
#
# The service runs as a *user* service so inotify watches the user's files with
# correct permissions and never needs root. The Notion token is supplied via
# EnvironmentFile (a 0600 file OUTSIDE the world-readable nix store). For stronger
# secret handling use systemd LoadCredential, agenix, or sops-nix and point
# environmentFile at the decrypted runtime path.

self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.notion-sync;
  defaultPackage = self.packages.${pkgs.system}.default;
in
{
  options.services.notion-sync = {
    enable = lib.mkEnableOption "the notion-sync two-way sync daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = defaultPackage;
      description = "The notion-sync package to run.";
    };

    configFile = lib.mkOption {
      type = lib.types.path;
      description = "Path to the TOML config file. Read at runtime, not copied into the store.";
    };

    environmentFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to a 0600 EnvironmentFile that defines NOTION_TOKEN=...
        MUST NOT be a nix store path (the store is world-readable).
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "RUST_LOG / tracing EnvFilter directive.";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.notion-sync = {
      description = "Two-way Notion <-> codebase sync daemon";
      documentation = [ "https://example.com/notion-sync" ];
      wantedBy = [ "default.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment = {
        RUST_LOG = cfg.logLevel;
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/notion-sync --config ${cfg.configFile}";
        # NOTION_TOKEN lives here, outside the nix store.
        EnvironmentFile = cfg.environmentFile;
        Restart = "on-failure";
        RestartSec = 5;
        # SIGTERM triggers graceful shutdown in the daemon; give it room to drain.
        KillSignal = "SIGTERM";
        TimeoutStopSec = 30;
        # Light hardening (user service).
        NoNewPrivileges = true;
        ProtectKernelTunables = true;
        ProtectControlGroups = true;
      };
    };

    # Reminder for headless boxes: `loginctl enable-linger <user>` so the user
    # service runs without an active login session. Not enforced here because it
    # is a per-user/system policy decision.
  };
}

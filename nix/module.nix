# systemd *user* service module for notion-sync (NixOS).
#
# Usage:
#
#   imports = [ inputs.notion-sync.nixosModules.default ];
#   services.notion-sync = {
#     enable = true;
#     environmentFile = config.sops.secrets."notion-token".path; # 0600, NOTION_TOKEN=...
#     settings.mapping = [
#       { local_root = "/home/alice/projects/app";
#         parent_page_id = "0123...";
#         ignore = [ ".git" "target" ]; }
#     ];
#   };
#
# The service runs as a *user* service so inotify watches the user's files with
# correct permissions and never needs root. The Notion token is supplied via
# EnvironmentFile (a 0600 file OUTSIDE the world-readable nix store); for stronger
# secret handling point it at an agenix/sops-nix decrypted runtime path.
#
# Config is rendered from `settings` into a store file and passed via `--config`.
# That store file holds NO secrets (the token only ever comes from the env), so a
# world-readable store path is fine. Set `configFile` instead to manage the TOML
# yourself. Per-directory `.notion-sync.toml` files live in the user's home, so
# they are handled by the home-manager / hjem modules, not here.

self:
{ config, lib, pkgs, ... }:

let
  cfg = config.services.notion-sync;
  helpers = import ./lib.nix { inherit lib pkgs; };

  configPath =
    if cfg.configFile != null then
      toString cfg.configFile
    else
      toString (helpers.renderConfig "notion-sync-config.toml" cfg.settings);
in
{
  options.services.notion-sync = {
    enable = lib.mkEnableOption "the notion-sync two-way sync daemon";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "notion-sync.packages.\${system}.default";
      description = "The notion-sync package to run.";
    };

    settings = helpers.settingsOption;
    configFile = helpers.configFileOption;

    environmentFile = lib.mkOption {
      type = lib.types.path;
      description = ''
        Path to a 0600 EnvironmentFile that defines NOTION_TOKEN=...
        MUST NOT be a nix store path (the store is world-readable).
      '';
    };
  } // helpers.logOptions // helpers.keepWarmOptions;

  config = lib.mkIf cfg.enable {
    assertions = [
      {
        assertion = cfg.configFile != null || cfg.settings != { };
        message = "services.notion-sync: set either `settings` (preferred) or `configFile`.";
      }
    ];

    systemd.user.services.notion-sync = {
      description = "Two-way Notion <-> codebase sync daemon";
      documentation = [ "https://github.com/feltfomo/notion-sync" ];
      wantedBy = [ "default.target" ];
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];

      environment.RUST_LOG = cfg.logLevel;

      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/notion-sync ${helpers.execArgs configPath cfg}";
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

    # Keep-warm: a system oneshot + timer that pokes the public webhook URL so a
    # relay tunnel's ingress never goes cold and drops Notion's one-shot delivery.
    # A system service (not user) with DynamicUser: it only curls a public URL, so
    # it needs neither the login session nor the user's files, and staying off the
    # user manager means lingering can't gate it.
    systemd.services.notion-sync-keepwarm = lib.mkIf cfg.keepWarm.enable {
      description = "Keep the notion-sync webhook tunnel warm";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      path = [ pkgs.curl ]
        ++ lib.optionals cfg.keepWarm.forcePublicPath [ pkgs.dnsutils pkgs.gnused ];
      serviceConfig = {
        Type = "oneshot";
        DynamicUser = true;
      };
      script = helpers.keepWarmScript cfg.keepWarm;
    };

    systemd.timers.notion-sync-keepwarm = lib.mkIf cfg.keepWarm.enable {
      description = "Periodic keep-warm ping for the notion-sync webhook tunnel";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnBootSec = "1min";
        OnUnitActiveSec = cfg.keepWarm.interval;
        AccuracySec = "5s";
      };
    };

    # Reminder for headless boxes: `loginctl enable-linger <user>` so the user
    # service runs without an active login session. Not enforced here because it
    # is a per-user/system policy decision.
  };
}
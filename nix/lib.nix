# Shared helpers for the notion-sync Nix modules.
#
# Pure: depends only on the `lib` and `pkgs` each caller hands in, so the NixOS,
# home-manager, and hjem modules all render config the same way and never drift.
# Keep this free of `self`/system assumptions -- the file-only hjem module imports
# it too, and it has no package to build.
{ lib, pkgs }:

let
  tomlFormat = pkgs.formats.toml { };

  # Recursively drop null-valued attributes. Optional settings default to null in
  # Nix so we can omit them; `key = null` is not valid TOML, so prune before
  # rendering. (Lists are passed through untouched -- omit optional keys instead
  # of setting them null inside a `mapping` entry.)
  pruneNulls = lib.filterAttrsRecursive (_name: value: value != null);
in
{
  inherit tomlFormat;

  # Render a settings attrset to a TOML store file. The Notion token is never part
  # of this file (it only ever comes from the environment), so a world-readable
  # store path is fine here.
  renderConfig = name: settings: tomlFormat.generate name (pruneNulls settings);

  # `settings`: freeform, mirrors config.example.toml 1:1.
  settingsOption = lib.mkOption {
    type = tomlFormat.type;
    default = { };
    example = {
      poll_interval_secs = 45;
      mapping = [
        {
          local_root = "/home/alice/projects/app";
          parent_page_id = "0123456789abcdef0123456789abcdef";
          ignore = [ ".git" "target" ];
        }
      ];
    };
    description = ''
      Declarative contents of config.toml, mirroring the schema in
      config.example.toml exactly (top-level keys plus one or more `mapping`
      entries). Ignored when `configFile` is set.
    '';
  };

  # `configFile`: escape hatch for anyone who'd rather hand-manage the TOML.
  configFileOption = lib.mkOption {
    type = lib.types.nullOr lib.types.path;
    default = null;
    description = ''
      Use this existing TOML file instead of rendering `settings`. The Notion
      token is never read from it.
    '';
  };

  # `perDirectory`: per-tree `.notion-sync.toml` overrides, keyed by a path
  # relative to $HOME (home-manager / hjem only). Only `ignore` and
  # `max_file_bytes` are honored by the daemon; everything else is rejected.
  perDirectoryOption = lib.mkOption {
    type = lib.types.attrsOf tomlFormat.type;
    default = { };
    example = {
      "projects/app/vendor" = {
        ignore = [ "*.generated" ];
        max_file_bytes = 20000000;
      };
    };
    description = ''
      Per-directory `.notion-sync.toml` files to materialize, keyed by a path
      relative to $HOME. Only `ignore` (added to the central list) and
      `max_file_bytes` are honored.
    '';
  };

  # Shared CLI/log knobs. Spread into each module's option set with `//`.
  logOptions = {
    color = lib.mkOption {
      type = lib.types.enum [ "auto" "always" "never" ];
      default = "auto";
      description = "ANSI color policy (`--color`). `auto` colors only on a TTY.";
    };

    logTime = lib.mkOption {
      type = lib.types.enum [ "datetime" "uptime" "none" ];
      default = "datetime";
      description = ''
        Timestamp style (`--log-time`). `datetime` includes seconds; `uptime` is
        seconds-since-start; `none` is best under journald (it stamps lines).
      '';
    };

    logLevel = lib.mkOption {
      type = lib.types.str;
      default = "info";
      description = "RUST_LOG / tracing EnvFilter directive.";
    };
  };

  # Daemon argv: `--config` plus the global log flags. The binary defaults to the
  # sync daemon when no subcommand is given.
  execArgs = configPath: cfg:
    lib.escapeShellArgs [
      "--config" (toString configPath)
      "--color" cfg.color
      "--log-time" cfg.logTime
    ];
}

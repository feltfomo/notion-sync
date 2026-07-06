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

  # `keepWarm`: an optional systemd timer that periodically pokes the public
  # webhook URL so a relay-based tunnel's edge<->node link never goes idle. Notion
  # delivers each event one-shot, and some tunnels -- Tailscale Funnel most visibly
  # -- let an idle ingress go cold: the first request after the idle window 502s
  # while the relay reconnects, then works. That silently drops the event (the
  # poller re-pulls it a cycle later) and, worse, fails subscription verification,
  # which is itself a single one-shot POST. Off by default; a cloudflared named
  # tunnel holds the connection open and never needs it.
  keepWarmOptions = {
    keepWarm = {
      enable = lib.mkEnableOption ''
        a periodic keep-warm ping of the public webhook URL, for relay-based
        tunnels (e.g. Tailscale Funnel) whose ingress goes cold when idle'';

      url = lib.mkOption {
        type = lib.types.str;
        example = "https://host.tailnet.ts.net/notion-webhook";
        description = ''
          Public webhook URL to poke -- the exact URL the subscription points at.
          A plain GET is enough: the receiver 404s it on method and the round trip
          is all that keeps the tunnel warm.
        '';
      };

      interval = lib.mkOption {
        type = lib.types.str;
        default = "45s";
        description = ''
          systemd OnUnitActiveSec between pings. Keep it under the tunnel's idle
          window; Tailscale's is short, so 45s is the safe default.
        '';
      };

      forcePublicPath = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Force the ping across the public relay instead of a local shortcut.
          Tailscale Funnel needs this: a request from the node itself takes the
          direct tailnet path and never crosses the ingress, so it can't warm it.
          When set, the URL's host is resolved against `resolver` (a PUBLIC DNS
          server -- MagicDNS would hand back the 100.x tailnet IP) and each edge
          IP is hit with `curl --resolve`. Leave false for a tunnel that doesn't
          shortcut (cloudflared), where a plain curl already crosses the edge.
        '';
      };

      resolver = lib.mkOption {
        type = lib.types.str;
        default = "1.1.1.1";
        description = "Public DNS resolver used when `forcePublicPath` is set.";
      };
    };
  };

  # Keep-warm shell script for a resolved `keepWarm` submodule. Shared so the NixOS
  # system timer and any future home-manager user timer render identical behavior.
  keepWarmScript = kw:
    if kw.forcePublicPath then ''
      url=${lib.escapeShellArg kw.url}
      host=$(printf '%s' "$url" | sed -E 's#^[a-z]+://([^/:]+).*#\1#')
      relays=$(dig +short @${kw.resolver} "$host" A)
      if [ -z "$relays" ]; then
        echo "notion-sync keep-warm: no public IPs for $host; skipping"
        exit 0
      fi
      for ip in $relays; do
        curl -sS -o /dev/null --max-time 10 --resolve "$host:443:$ip" "$url" || true
      done
    '' else ''
      curl -sS -o /dev/null --max-time 10 ${lib.escapeShellArg kw.url} || true
    '';

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

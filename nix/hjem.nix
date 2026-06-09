# hjem module for notion-sync (config only).
#
# hjem manages files in $HOME; it does NOT define systemd services (see
# feel-co/hjem#63). So this module materializes config -- the central config.toml
# and any per-directory .notion-sync.toml files -- and leaves the daemon to the
# NixOS user-service module (or home-manager). Evaluate it inside a
# `hjem.users.<name>` submodule:
#
#   hjem.users.alice = {
#     imports = [ inputs.notion-sync.hjemModules.default ];
#     programs.notion-sync = {
#       enable = true;
#       settings.mapping = [
#         { local_root = "/home/alice/projects/app"; parent_page_id = "0123..."; }
#       ];
#       perDirectory."projects/app/vendor".ignore = [ "*.generated" ];
#     };
#   };
#
# The daemon itself still comes from nixosModules.notion-sync (point its
# `configFile` at ~/.config/notion-sync/config.toml, or just let both render the
# same `settings`).

{ config, lib, pkgs, ... }:

let
  cfg = config.programs.notion-sync;
  helpers = import ./lib.nix { inherit lib pkgs; };
in
{
  options.programs.notion-sync = {
    enable = lib.mkEnableOption "notion-sync config files managed via hjem (config only)";

    settings = helpers.settingsOption;
    configFile = helpers.configFileOption;
    perDirectory = helpers.perDirectoryOption;
  };

  config = lib.mkIf cfg.enable {
    # ~/.config/notion-sync/config.toml
    xdg.config.files = lib.mkIf (cfg.configFile == null) {
      "notion-sync/config.toml".source =
        helpers.renderConfig "notion-sync-config.toml" cfg.settings;
    };

    # Per-directory .notion-sync.toml files (keys are relative to $HOME).
    files = lib.mapAttrs'
      (dir: value:
        lib.nameValuePair "${dir}/.notion-sync.toml" {
          source = helpers.renderConfig "notion-sync-perdir.toml" value;
        })
      cfg.perDirectory;
  };
}

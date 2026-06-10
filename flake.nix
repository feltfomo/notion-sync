{
  description = "notion-sync: two-way sync daemon mirroring a local codebase into Notion";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    let
      # System-independent outputs: the NixOS, home-manager, and hjem modules.
      moduleOutputs = {
        nixosModules.default = import ./nix/module.nix self;
        nixosModules.notion-sync = import ./nix/module.nix self;

        homeManagerModules.default = import ./nix/home-manager.nix self;
        homeManagerModules.notion-sync = import ./nix/home-manager.nix self;

        # hjem manages files only, so this one needs no `self`/package.
        hjemModules.default = import ./nix/hjem.nix;
        hjemModules.notion-sync = import ./nix/hjem.nix;
      };
    in
    moduleOutputs // flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "notion-sync";
          version = "0.5.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;

          # rustls-tls + bundled sqlite => no openssl/sqlite system deps to wire up.
          doCheck = true;

          meta = with pkgs.lib; {
            description = "Two-way sync daemon mirroring a local codebase into a Notion page tree";
            license = licenses.mit;
            mainProgram = "notion-sync";
            platforms = platforms.linux;
          };
        };

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/notion-sync";
        };

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy pkgs.rustfmt ];
        };

        formatter = pkgs.nixpkgs-fmt;
      });
}

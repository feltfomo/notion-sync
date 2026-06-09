# notion-sync

A background daemon that keeps a local directory and a Notion page tree in two-way sync.
The local filesystem is the source of truth (local-wins): Notion is a mirror you can read,
comment on, and lightly edit from anywhere. On a conflict, the file on disk wins.

Mirrors any UTF-8 text file, not just code. Markdown, config, prose, whatever. Each file's
bytes are wrapped in Notion code blocks so they round-trip exactly.

## Install

No system dependencies to chase down: TLS is rustls (no OpenSSL) and SQLite is bundled, so a
single binary is all you need. Start with Option A. The rest are here if you'd rather not pipe
a script into a shell, or want to build it yourself.

Downloading is the easy part. This is a developer tool, so finishing setup still means creating
a Notion integration token and editing a small config file (see [Quickstart](#quickstart)).

### Option A: one-line installer (easiest)

Paste this into a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/feltfomo/notion-sync/main/scripts/install.sh | sh
```

It picks the right build, downloads `notion-sync` and `fidelity-probe`, verifies them, and
installs to `~/.local/bin`. If that folder isn't on your `PATH`, it prints the one line to fix
that.

Want to read it before piping it into a shell? Fair:
<https://github.com/feltfomo/notion-sync/blob/main/scripts/install.sh>

### Option B: download by hand (no script)

1. Open the latest release: <https://github.com/feltfomo/notion-sync/releases/latest>
2. Under **Assets**, grab these two (the `musl` builds are static and run on any Linux; pick
   these when in doubt):
   - `notion-sync-x86_64-unknown-linux-musl`
   - `fidelity-probe-x86_64-unknown-linux-musl`
3. Make them runnable and put them on your `PATH`:

   ```sh
   mkdir -p ~/.local/bin
   mv notion-sync-x86_64-unknown-linux-musl    ~/.local/bin/notion-sync
   mv fidelity-probe-x86_64-unknown-linux-musl ~/.local/bin/fidelity-probe
   chmod +x ~/.local/bin/notion-sync ~/.local/bin/fidelity-probe
   ```

4. If `~/.local/bin` isn't on your `PATH`, add it (bash shown):

   ```sh
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
   ```

On an unusual setup, the `-gnu` files are the smaller glibc build and are otherwise identical.
To check nothing got mangled in transit, download `SHA256SUMS` into the same folder and run
`sha256sum --check --ignore-missing SHA256SUMS`.

### Option C: build it yourself

With cargo (needs Rust >= 1.74):

```sh
cargo install --git https://github.com/feltfomo/notion-sync --tag v0.2.0
```

From a checkout:

```sh
git clone https://github.com/feltfomo/notion-sync
cd notion-sync
cargo build --release
# binaries land in ./target/release/{notion-sync,fidelity-probe}
```

For a static musl binary, build with [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild)
(it handles the bundled SQLite cross-compile):

```sh
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Or run it once without installing:

```sh
nix run github:feltfomo/notion-sync
```

On NixOS, don't `nix profile install` it. Use the flake input and module below.

## Quickstart

On first run with no config, the daemon writes a starter `~/.config/notion-sync/config.toml`,
prints which fields to edit, and exits.

1. Create a Notion internal integration, copy its token, and **share the parent page with the
   integration** so it has write access.
2. Export the token (it's never read from the config file):

   ```sh
   set -x NOTION_TOKEN ntn_xxx     # fish
   export NOTION_TOKEN=ntn_xxx     # bash / zsh
   ```

3. Scaffold and edit the config:

   ```sh
   notion-sync                                   # writes config.toml, then exits
   $EDITOR ~/.config/notion-sync/config.toml     # set local_root + parent_page_id
   ```

4. **Run the fidelity gate first** (see below) against a scratch page.
5. Start the daemon:

   ```sh
   notion-sync --config ~/.config/notion-sync/config.toml
   ```

## How it works

```text
local file saved  ──(debounce 750–2000ms)──▶  watcher ─▶ engine ─▶ Notion page
Notion page edited ──(poll every 45s)──────▶  poller ─▶ conflict (local-wins) ─▶ local file

            state.db (SQLite, machine-local) tracks the file ⇄ page mapping
```

- **Folders** become subpages. **Files** become subpages whose body is the file content
  wrapped in syntax-highlighted code blocks.
- File contents are **chunked** to respect Notion limits: 2000 UTF-16 units per rich-text item
  (the count Notion enforces, not Rust `char`s), 100 items per code block, 100 children per
  append. Chunking is positional, so reassembly is byte-exact.
- **Page title = filename.** Renames are detected by content hash and applied with
  `PATCH /v1/pages/{id}`, so a page keeps its comments and history across a `git mv` instead of
  being trashed and recreated.
- **Binary or oversized files** get a placeholder page with a warning callout and are never
  written back.
- A shared token-bucket rate limiter (~3 req/s) covers the watcher and poller. 429s honor
  `Retry-After`; 409/5xx use exponential backoff with jitter.
- Startup **reconciliation** adopts existing pages by title rather than recreating them, and
  refuses to run on a missing or empty local tree so a transient glitch can't wipe the mirror.

## Fidelity gate (run this first)

Write-back is only safe if Notion preserves bytes exactly. Before trusting the daemon, run the
probe against a real workspace:

```sh
NOTION_TOKEN=ntn_xxx cargo run --bin fidelity-probe -- --parent-page-id <PAGE_ID>
```

It writes an adversarial payload (tabs, mixed indentation, trailing whitespace, a blank line,
emoji/CJK/€/é, a >2000-char multibyte run, a final newline) through the real `/v1/blocks` API,
reads it back through the same chunker, and **exits non-zero if a single byte differs**. If
Notion ever mutates content, the chunker has to compensate deterministically before write-back
is enabled.

## Configuration (`config.toml`)

The token is **never** in this file. It comes from `$NOTION_TOKEN`. See `config.example.toml`
for the annotated template.

```toml
notion_version     = "2022-06-28"
poll_interval_secs = 45
debounce_ms        = 1000      # must be within [750, 2000]
conflict_policy    = "local-wins"
max_file_bytes     = 5000000

[mapping]
local_root     = "/home/you/project"
parent_page_id = "0123456789abcdef0123456789abcdef"
ignore         = [".git", "target", "node_modules", "*.lock", ".notion-sync"]
```

## NixOS (flake input + systemd service)

Add it as a flake input and use the bundled module. `flake.nix` exposes a `buildRustPackage`
package, a `nix run` app, and `nixosModules.default`. The token comes from `EnvironmentFile=`
(a `0600` secrets file, or agenix/sops-nix) and never touches the Nix store.

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    notion-sync.url = "github:feltfomo/notion-sync";
    notion-sync.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, notion-sync, ... }: {
    nixosConfigurations.my-host = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        notion-sync.nixosModules.default
        {
          services.notion-sync = {
            enable          = true;
            configFile      = "/home/you/.config/notion-sync/config.toml";
            environmentFile = "/run/secrets/notion-sync.env"; # NOTION_TOKEN=...
          };
        }
      ];
    };
  };
}
```

Then `nixos-rebuild switch`. Update with `nix flake update notion-sync`.

Options under `services.notion-sync`:

| Option | Type | Default | What it does |
| --- | --- | --- | --- |
| `enable` | bool | `false` | Turn the user service on. |
| `package` | package | the flake's `notion-sync` | The build to run; override to pin or patch. |
| `configFile` | path | _required_ | Absolute path to your `config.toml`, passed as `--config`. |
| `environmentFile` | path | _required_ | `0600` file holding `NOTION_TOKEN=...`, loaded by systemd at start. |
| `logLevel` | string | `"info"` | `RUST_LOG` for the unit: `error`, `warn`, `info`, `debug`, or `trace`. |

It runs as a **user service**, restarts on failure, and stops cleanly on SIGTERM. On a headless
box it needs lingering to run without an active login: set `users.users.<name>.linger = true;`,
or run `loginctl enable-linger <user>`.

## State & limitations

- `state.db` lives under `$XDG_STATE_HOME/notion-sync/` (falls back to
  `~/.local/state/notion-sync/`).
- Conflict backups go to `<local_root>/.notion-sync/conflicts/`.
- **Markdown caveat:** edit `.md` files locally, not in Notion's block editor. A file's bytes
  live inside one code block, so a structured edit of a Markdown file (with its own code fences)
  can split the mirror. The daemon detects this and force-pushes local instead, but treat `.md`
  mirror pages as read-only and save yourself the trouble.
- **Multi-machine is out of scope for v1.** `state.db` is deliberately local. Don't run two
  daemons against the same Notion tree from different machines.
- Trashed-block cleanup isn't implemented. Notion auto-purges trash after 30 days anyway.

## Layout

```text
src/
  api/        rate-limited retrying client + serde models + shared token bucket
  chunk.rs    positional chunker/reassembler (fidelity-critical)
  language.rs extension -> Notion code-block language
  hashutil.rs blake3 hashing (change + rename detection)
  config.rs   TOML loader + ignore globs
  state.rs    SQLite state.db
  sync/       engine, watcher, poller, conflict, reconcile, locks, util
  lib.rs      crate root that wires the modules together
  main.rs     daemon entrypoint (reconcile -> watcher + poller, SIGTERM)
  bin/fidelity_probe.rs   the standalone fidelity gate
nix/module.nix  NixOS user-service module
flake.nix       package + run app + NixOS module outputs
```

Unit tests live inline as `#[cfg(test)]` modules (see `chunk.rs`, `state.rs`). The fidelity
probe is the live, credential-gated check, not a `cargo test` target.

## License

MIT. See [LICENSE](./LICENSE).

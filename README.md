# notion-sync

A background daemon that keeps a local directory and a Notion page tree in two-way
sync. **The local filesystem is the source of truth** (local-wins). Notion becomes a
mirror you can read, comment on, and lightly edit from anywhere. The files on disk
always win.

It mirrors any UTF-8 text file, not just code: Markdown, config, prose, whatever.
Each file's bytes get wrapped in Notion code blocks so they round-trip exactly.

---

## Install

There's no system dependency to chase down: TLS is rustls (no OpenSSL) and SQLite is
bundled, so a single binary is all you need. Start with Option A; the others are there
if you'd rather not run a script, or want to build it yourself.

> **Note:** the *download* is easy, but `notion-sync` is a developer tool — finishing
> setup still means creating a Notion integration token and editing a small config
> file (see [Quickstart](#quickstart)). The steps below just get the program onto your
> machine painlessly.

### Option A — one-line installer (easiest)

test

Paste this into a terminal:

```sh
curl -fsSL https://raw.githubusercontent.com/feltfomo/notion-sync/main/scripts/install.sh | sh
```

It picks the right build, downloads `notion-sync` and `fidelity-probe`, verifies they
downloaded cleanly, and installs them to `~/.local/bin`. If that folder isn't on your
`PATH` yet, it prints the one line to fix that. Done.

Want to read it before trusting it? Have a look first:
<https://github.com/feltfomo/notion-sync/blob/main/scripts/install.sh>

### Option B — download by hand (no script)

1. Open the latest release: <https://github.com/feltfomo/notion-sync/releases/latest>
2. Under **Assets**, download these two files (the `musl` builds are static and run on
   any Linux — when in doubt, pick these):
   - `notion-sync-x86_64-unknown-linux-musl`
   - `fidelity-probe-x86_64-unknown-linux-musl`
3. Open a terminal in the folder you downloaded them to, then make them runnable and
   move them somewhere your shell looks for programs:

   ```sh
   mkdir -p ~/.local/bin
   mv notion-sync-x86_64-unknown-linux-musl    ~/.local/bin/notion-sync
   mv fidelity-probe-x86_64-unknown-linux-musl ~/.local/bin/fidelity-probe
   chmod +x ~/.local/bin/notion-sync ~/.local/bin/fidelity-probe
   ```

4. If `~/.local/bin` isn't on your `PATH`, add it once (bash shown — use your shell's
   rc file):

   ```sh
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
   ```

Prefer the smaller glibc build, or on an unusual setup? Grab the `-gnu` files instead
— everything else is identical. And to confirm nothing got corrupted in transit,
download `SHA256SUMS` into the same folder and run
`sha256sum --check --ignore-missing SHA256SUMS`.

### Option C — build it yourself

With cargo (any distro; needs a Rust toolchain ≥ 1.74, e.g. `rustup` or `pacman -S rust`):

```sh
cargo install --git https://github.com/feltfomo/notion-sync --tag v0.2.0
```

From a local checkout:

```sh
git clone https://github.com/feltfomo/notion-sync
cd notion-sync
cargo build --release
# binaries land in ./target/release/{notion-sync,fidelity-probe}
```

For a fully static binary yourself, build the musl target with
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild) (it handles the
bundled SQLite C cross-compile):

```sh
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Or just try it once with Nix, no install:

```sh
nix run github:feltfomo/notion-sync
```

On NixOS, don't `nix profile install` it (that's imperative and doesn't carry across
machines) — add it as a **flake input** and enable the module instead. See
[NixOS (flake input + systemd service)](#nixos-flake-input--systemd-service) below.

## Quickstart

On first run with no config, the daemon writes a starter
`~/.config/notion-sync/config.toml`, prints which fields to edit, and exits.

1. Create a Notion internal integration, copy its token, and **share the parent page
   with the integration** so it has write access.
2. Export the token (it's never read from the config file):

   ```sh
   set -x NOTION_TOKEN ntn_xxx     # fish
   export NOTION_TOKEN=ntn_xxx     # bash / zsh
   ```

3. Scaffold and edit the config:

   ```sh
   notion-sync                                   # writes ~/.config/notion-sync/config.toml, then exits
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

- **Folders** become subpages. **Files** become subpages whose body is the file
  content wrapped in syntax-highlighted **code blocks**.
- File contents are **chunked** to respect Notion limits: 2000 UTF-16 units per
  rich-text item (the count Notion enforces, not Rust `char`s), 100 items per code
  block, 100 children per append request. Chunking is purely positional, so
  reassembly is byte-exact.
- **Page title = filename.** Renames are detected by **content hash** and applied
  with `PATCH /v1/pages/{id}` (rename + reparent), so a page keeps its comments and
  history across a `git mv` or refactor instead of being trashed and recreated.
- **Binary / oversized files** get a placeholder page with a ⚠️ warning callout and
  are never written back.
- A shared **token-bucket rate limiter** (~3 req/s) is used by both the watcher and
  poller; 429s honor `Retry-After`, and 409/5xx use exponential backoff + jitter.
- Startup **reconciliation** adopts existing pages by title (it never blindly
  recreates), and refuses to run if the local tree is missing or empty so a transient
  glitch can't mass-delete the mirror.

## Fidelity gate (run this first)

Write-back is only safe if Notion preserves bytes exactly. Before trusting the
daemon, run the standalone probe against a real workspace:

```sh
NOTION_TOKEN=ntn_xxx cargo run --bin fidelity-probe -- --parent-page-id <PAGE_ID>
```

It writes an adversarial payload (tabs, mixed indentation, trailing whitespace, a
blank line, emoji/CJK/€/é, a >2000-char multibyte run, a final newline) through the
real `/v1/blocks` API, reads it back through the same chunker, and **exits non-zero
if a single byte differs.** If Notion ever mutates content, the chunker must
compensate deterministically before write-back is enabled.

## Configuration (`config.toml`)

The Notion token is **never** in this file; it comes from `$NOTION_TOKEN`. See
`config.example.toml` for the annotated template.

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

On NixOS the right way to install this is as a **flake input** plus the bundled NixOS
module — declarative, pinned by your `flake.lock`, and reproducible on every machine you
deploy to. (`flake.nix` exposes a `buildRustPackage` package, a `nix run` app, and
`nixosModules.default`.) The token is provided via `EnvironmentFile=` pointing at a
`0600` secrets file (or `agenix` / `sops-nix`), and is **never** placed in the Nix store.

Add the input to your system flake and wire the module into the host:

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    notion-sync.url = "github:feltfomo/notion-sync";
    # reuse your nixpkgs so a second copy isn't built
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
            environmentFile = "/run/secrets/notion-sync.env"; # contains NOTION_TOKEN=...
          };
        }
      ];
    };
  };
}
```

Then `sudo nixos-rebuild switch`. Upgrade deliberately with
`nix flake update notion-sync` (and roll back via your `flake.lock` like anything else).

Module options, all under `services.notion-sync`:

| Option | Type | Default | What it does |
| --- | --- | --- | --- |
| `enable` | bool | `false` | Turn the user service on. |
| `package` | package | the flake's `notion-sync` | The build to run; override to pin or patch. |
| `configFile` | path | _required_ | Absolute path to your `config.toml`, passed as `--config`. |
| `environmentFile` | path | _required_ | `0600` file holding `NOTION_TOKEN=...`, loaded by systemd at start and never written to the Nix store. |
| `logLevel` | string | `"info"` | Sets `RUST_LOG` for the unit: `error`, `warn`, `info`, `debug`, or `trace`. |

The unit runs as a **user service** (so `$HOME` / `$XDG_STATE_HOME` resolve to your
account), restarts on failure, and shuts down gracefully on `SIGTERM`. On a headless
box it needs lingering to run without an active login session: set
`users.users.<name>.linger = true;` declaratively, or run `loginctl enable-linger <user>`.

## State & limitations

- `state.db` lives under `$XDG_STATE_HOME/notion-sync/` (falls back to
  `~/.local/state/notion-sync/`).
- Conflict backups are written to `<local_root>/.notion-sync/conflicts/`.
- **Markdown caveat:** edit `.md` files locally, not in Notion's block editor. Because
  a file's bytes live inside a single code block, a structured edit of a Markdown file
  (which carries its own code fences) can split the mirror. The daemon detects this
  and refuses the pull (force-pushing local instead), but the safe habit is to treat
  `.md` mirror pages as read-only.
- **Multi-machine is out of scope for v1.** `state.db` is intentionally local; don't
  run two daemons against the same Notion tree from different machines.
- Trashed-block cleanup isn't implemented; Notion auto-purges trash after 30 days anyway.

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

Unit tests live inline as `#[cfg(test)]` modules (see `chunk.rs`, `state.rs`); the
fidelity probe is the live, credential-gated check, not a `cargo test` target.

## License

MIT. See [LICENSE](./LICENSE).

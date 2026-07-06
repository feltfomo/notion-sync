# notion-sync

A background daemon that keeps local directories and their Notion page trees in two-way sync. The filesystem is the source of truth: Notion is a mirror you can read, comment on, and lightly edit. On a conflict, the local file wins.

Works on any UTF-8 text file, not just code. Each file's bytes are wrapped in Notion code blocks so they round-trip exactly.

## Install

TLS is rustls (no OpenSSL) and SQLite is bundled, so the binary is self-contained. Setup still requires a Notion integration token and a small config file (see [Quickstart](#quickstart)).

### Option A: install script

```sh
curl -fsSL https://raw.githubusercontent.com/feltfomo/notion-sync/main/scripts/install.sh | sh
```

Picks the right build, downloads `notion-sync` and `fidelity-probe`, verifies them, and installs to `~/.local/bin`. Source: <https://github.com/feltfomo/notion-sync/blob/main/scripts/install.sh>

### Option B: manual download

1. Open the latest release: <https://github.com/feltfomo/notion-sync/releases/latest>
2. Under **Assets**, download the `musl` builds (static, run on any Linux):
   - `notion-sync-x86_64-unknown-linux-musl`
   - `fidelity-probe-x86_64-unknown-linux-musl`
3. Install them:

   ```sh
   mkdir -p ~/.local/bin
   mv notion-sync-x86_64-unknown-linux-musl    ~/.local/bin/notion-sync
   mv fidelity-probe-x86_64-unknown-linux-musl ~/.local/bin/fidelity-probe
   chmod +x ~/.local/bin/notion-sync ~/.local/bin/fidelity-probe
   ```

4. Add `~/.local/bin` to your `PATH` if needed:

   ```sh
   echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
   ```

The `-gnu` builds are smaller glibc binaries and otherwise identical. To verify downloads, fetch `SHA256SUMS` into the same folder and run `sha256sum --check --ignore-missing SHA256SUMS`.

### Option C: build from source

```sh
cargo install --git https://github.com/feltfomo/notion-sync --tag v0.6.0
```

From a checkout:

```sh
git clone https://github.com/feltfomo/notion-sync
cd notion-sync
cargo build --release
# binaries in ./target/release/{notion-sync,fidelity-probe}
```

For a static musl binary, use [`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild):

```sh
rustup target add x86_64-unknown-linux-musl
cargo zigbuild --release --target x86_64-unknown-linux-musl
```

Run without installing:

```sh
nix run github:feltfomo/notion-sync
```

On NixOS, use the flake input and module below rather than `nix profile install`.

## Quickstart

On first run with no config, the daemon writes a starter `~/.config/notion-sync/config.toml`, prints the fields to edit, and exits.

1. Create a Notion internal integration, copy its token, and share the parent page with the integration so it has write access.
2. Export the token:

   ```sh
   export NOTION_TOKEN=ntn_xxx     # bash / zsh
   set -x NOTION_TOKEN ntn_xxx     # fish
   ```

   Or point `token_file` in the config at a file holding the token. `$NOTION_TOKEN` takes priority; `token_file` is read only when it's unset or empty.
3. Edit the config:

   ```sh
   notion-sync                                   # writes config.toml, then exits
   $EDITOR ~/.config/notion-sync/config.toml     # set local_root + parent_page_id
   ```

4. Start the daemon:

   ```sh
   notion-sync --config ~/.config/notion-sync/config.toml
   ```

## How it works

```text
local file saved    ──(debounce ~1s)──────▶  watcher  ─▶ engine ─▶ Notion page
Notion page edited  ──(webhook, seconds)──▶  receiver ─┐
                    ──(poll, 45s)──────────▶  poller   ─┴▶ conflict (local-wins) ─▶ local file

            state.db (SQLite, machine-local) tracks the file ⇄ page mapping
```

- Folders become subpages. Files become subpages whose body is the file content in syntax-highlighted code blocks.
- File contents are chunked to respect Notion limits (2000 UTF-16 units per rich-text item, 100 items per code block, 100 children per append). Chunking is positional, so reassembly is byte-exact.
- Page title is the filename. Renames are detected by content hash and applied with `PATCH /v1/pages/{id}`, so a page keeps its comments and history across a `git mv`.
- Binary or oversized files get a placeholder page and are never written back.
- Symlinks are skipped, never followed.
- A shared token-bucket rate limiter (~3 req/s) covers the watcher and poller. 429s honor `Retry-After`; 409/5xx use exponential backoff with jitter.
- Startup reconciliation adopts existing pages by title, and skips any mapping whose local tree is missing or empty so a transient glitch can't wipe its mirror.
- A page created directly in Notion under a mapped parent is adopted as a local file once it reads back as a real code body. Empty pages, and pages split into non-code blocks, are skipped until they have one.
- With `[webhook]` on, Notion pushes edits to a local receiver so pulls land in seconds instead of waiting for the next poll; the poller stays on as the fallback. See [Webhooks](#webhooks-optional).
- Add a `[[mapping]]` block per directory to mirror multiple trees from one daemon.

## Fidelity check (optional)

Write-back is only safe if Notion preserves bytes exactly. `fidelity-probe` writes an adversarial payload (tabs, trailing whitespace, emoji/CJK, a >2000-char multibyte run, a final newline) through the real API, reads it back through the chunker, and exits non-zero if any byte differs:

```sh
NOTION_TOKEN=ntn_xxx fidelity-probe --parent-page-id <PAGE_ID>
```

Run it against a scratch page before trusting the daemon on important files.

## Configuration (`config.toml`)

The token is never written in this file; it comes from `$NOTION_TOKEN` or the file named by `token_file`. See `config.example.toml` for the annotated template.

```toml
notion_version     = "2022-06-28"
poll_interval_secs = 45
debounce_ms        = 1000      # must be within [750, 2000]
conflict_policy    = "local-wins"
max_file_bytes     = 5000000

# Read only when $NOTION_TOKEN is unset or empty.
# token_file = "/run/secrets/notion-token"

[[mapping]]
name           = "project"
local_root     = "/home/you/project"
parent_page_id = "0123456789abcdef0123456789abcdef"
ignore         = [".git", "target", "node_modules", "*.lock", "result", "dist", ".notion-sync"]

[[mapping]]
name           = "notes"
local_root     = "/home/you/notes"
parent_page_id = "fedcba9876543210fedcba9876543210"
ignore         = [".git", ".notion-sync"]
```

`name` is optional (defaults to the last component of `local_root`) and must be unique. A single `[mapping]` table is also accepted. For the optional `[webhook]` table, see [Webhooks](#webhooks-optional).

### Per-directory overrides

Each mapped directory can carry a `.notion-sync.toml` in its root:

```toml
# /home/you/project/.notion-sync.toml
ignore         = ["build", "*.tmp"]   # added to the central ignore list
max_file_bytes = 20000000             # overrides the central cap for this directory
```

It honors only those two keys; anything else (`parent_page_id`, `local_root`, `token_file`) is rejected, so a mapping can't be repointed from inside its own tree. `.notion-sync.toml` and the `.notion-sync/` state dir are never mirrored.

## Webhooks (optional)

Polling every 45s works, but a remote edit can take most of that window to show up. Turn on the webhook receiver and Notion pushes the change instead, so it lands in seconds. The poller stays on as the fallback — Notion delivery is at-most-once and can be dropped, reordered, or batched — so this is a latency win, never the only path.

```toml
[webhook]
enabled            = true
bind               = "127.0.0.1"
port               = 8080
path               = "/notion-webhook"
fallback_poll_secs = 900
```

Notion only delivers to a public HTTPS URL, never to localhost, so the receiver binds loopback and expects something in front to terminate TLS and forward to it.

On first connect Notion posts a one-time `verification_token`; the daemon logs it and persists it under `$XDG_STATE_HOME/notion-sync/webhook_secret`, and you paste it back into the integration's **Webhooks** tab to verify. After that every event is checked (HMAC-SHA256 over the raw body), and the secret survives restarts, so you only verify once.

To pin the secret yourself instead of relying on the persisted handshake, set `$NOTION_WEBHOOK_SECRET` or point `secret_file` at a file (sops-nix / systemd `LoadCredential` friendly).

### Picking a tunnel

Notion pins the URL at verify time and auto-pauses a subscription that racks up errors, so the tunnel choice matters more than it looks: you want a **stable hostname** in front of a receiver that's **reliably up**. A hostname that changes out from under you is the fastest way to get paused.

- **`cloudflared tunnel --url` (quick tunnel)** — zero setup, but a new random `*.trycloudflare.com` hostname every run. Fine for a smoke test, useless once verified: the next restart strands Notion on a dead URL and the subscription pauses.

  ```sh
  cloudflared tunnel --url http://127.0.0.1:8080
  ```

- **Named Cloudflare tunnel** — stable hostname, but needs a Cloudflare account with a *zone* (a domain you've delegated to Cloudflare). No domain, no named tunnel.
- **Tailscale Funnel** — a stable public HTTPS hostname (`https://<host>.<tailnet>.ts.net/`) with no domain and no paid plan. If you already run Tailscale, it's the least-effort stable option:

  ```sh
  # one-time: enable HTTPS certs + Funnel for the tailnet in the admin console.
  # --bg persists the serve config across reboots.
  tailscale funnel --bg 8080
  ```

  The hostname is bound to the node, so it survives restarts and reboots — which is the whole point. Verify the subscription once against `https://<host>.<tailnet>.ts.net/notion-webhook` and you're done.

  One sharp edge, and it bit us: Funnel's ingress goes cold when idle. The first request after a quiet stretch returns a `502` while the relay re-establishes its link to your node, then works. Notion delivers each event one-shot, so a cold relay silently eats that event; the poller catches it a cycle later, but that's the exact latency you turned webhooks on to kill. It also breaks *subscription verification*, which is a single one-shot `POST`: the verify click fails with `502` on a cold relay and succeeds the moment you've warmed it (curl the URL once, then verify). Keep the ingress warm to fix it: on Nix use [`keepWarm`](#nixos--home-manager), otherwise any cron/timer that curls the URL every ~45s. One catch: a curl *from the node itself* won't do it, because it takes the direct tailnet path and never crosses the ingress. You have to force the public relay path, resolving the funnel name against a public DNS server (MagicDNS hands back the tailnet IP, not the edge) and hitting that edge address.

Whatever you pick, point the subscription at `https://<your-tunnel>/notion-webhook`. And mind the one failure mode that bit me: a tunnel only forwards from the edge to the node — it can't answer for a daemon that isn't running. A stopped daemon shows up as a `502` to Notion, and enough of those pause the subscription. Since this is a user service, keep it alive across logout with lingering (`loginctl enable-linger <user>`, or `users.users.<name>.linger = true;` on NixOS). If it does get paused, just resume — the poller has been covering the gap the whole time, so nothing is lost.

## NixOS / home-manager

`flake.nix` exposes a package, a `nix run` app, and three modules:

- `nixosModules.default` — the systemd *user* service.
- `homeManagerModules.default` — the same service plus config materialized in `$HOME` (the central `config.toml` and any per-directory `.notion-sync.toml`).
- `hjemModules.default` — config files only (hjem doesn't manage services); pair it with the NixOS module for the daemon.

The token comes from `environmentFile` and never touches the Nix store.

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
            environmentFile = "/run/secrets/notion-sync.env"; # NOTION_TOKEN=...
            settings.mapping = [
              { local_root     = "/home/you/project";
                parent_page_id = "0123456789abcdef0123456789abcdef";
                ignore         = [ ".git" "target" ]; }
            ];
          };
        }
      ];
    };
  };
}
```

Then `nixos-rebuild switch`. Update with `nix flake update notion-sync`.

`settings` mirrors `config.toml` one-to-one (top-level keys plus one or more `mapping` entries), renders to a store file, and is passed as `--config`. The rendered file holds no secrets, so a world-readable store path is fine. Prefer it; if you'd rather hand-manage the TOML, set `configFile` instead.

| Option | Type | Default | Description |
| --- | --- | --- | --- |
| `enable` | bool | `false` | Turn the user service on. |
| `package` | package | the flake's `notion-sync` | The build to run. |
| `settings` | attrs | `{}` | Declarative `config.toml` (mirrors `config.example.toml`). Ignored when `configFile` is set. |
| `configFile` | path | `null` | Use this existing TOML file instead of rendering `settings`. |
| `environmentFile` | path | _required_ | `0600` file holding `NOTION_TOKEN=...` (add `NOTION_WEBHOOK_SECRET=...` if you pin it). |
| `color` | enum | `"auto"` | ANSI color policy (`--color`): `auto`, `always`, `never`. |
| `logTime` | enum | `"datetime"` | Log timestamp style (`--log-time`): `datetime`, `uptime`, `none`. |
| `logLevel` | string | `"info"` | `RUST_LOG` directive. |
| `keepWarm.enable` | bool | `false` | Add a timer that pings the webhook URL so a relay tunnel's ingress stays warm. |
| `keepWarm.url` | string | _required if enabled_ | Public webhook URL to poke (the one the subscription points at). |
| `keepWarm.interval` | string | `"45s"` | Time between pings (`OnUnitActiveSec`); keep under the tunnel's idle window. |
| `keepWarm.forcePublicPath` | bool | `false` | Force the ping across the public relay (needed for Tailscale Funnel). |
| `keepWarm.resolver` | string | `"1.1.1.1"` | Public DNS resolver used when `forcePublicPath` is set. |

The home-manager and hjem modules add `perDirectory.<path>` for per-tree `.notion-sync.toml` overrides, keyed relative to `$HOME`. Set exactly one of `settings` or `configFile` (the module asserts it).

It runs as a user service and restarts on failure. On a headless box, enable lingering: `users.users.<name>.linger = true;` or `loginctl enable-linger <user>`. This matters most with webhooks on — a daemon that's down during a delivery returns errors Notion counts toward an auto-pause.

For webhooks under Nix, put the `[webhook]` table in `settings.webhook` and supply the signing secret via `environmentFile` (`NOTION_WEBHOOK_SECRET=...`) or let the one-time handshake persist it. The public-HTTPS tunnel is deployed separately — a `services.cloudflared` named tunnel, or Tailscale Funnel (`services.tailscale` + `tailscale funnel --bg 8080`); see [Picking a tunnel](#picking-a-tunnel).

On a relay tunnel (Tailscale Funnel), also set `services.notion-sync.keepWarm` so a cold ingress can't drop Notion's one-shot deliveries — see [Picking a tunnel](#picking-a-tunnel) for the why. It's off by default and adds a small system timer that pings the public URL on an interval:

```nix
services.notion-sync.keepWarm = {
  enable          = true;
  url             = "https://<host>.<tailnet>.ts.net/notion-webhook";
  forcePublicPath = true;   # Tailscale: force the request across the public relay
  # interval      = "45s";  # default; keep it under the tunnel's idle window
};
```

Leave `forcePublicPath` off for a cloudflared named tunnel — it holds the connection open, so a plain curl from the host already crosses the edge.

## State & limitations

- `state.db` lives under `$XDG_STATE_HOME/notion-sync/` (falls back to `~/.local/state/notion-sync/`). The content-addressed snapshot store sits beside it at `objects/`, so dedup spans every mapping.
- With multiple mappings, every path is namespaced as `<mapping>/<path>`. CLI subcommands (`log`, `show`, `restore`, ...) take these keys. A pre-0.3 `state.db` migrates automatically only when a single mapping is configured.
- Conflict backups go to `<local_root>/.notion-sync/conflicts/`.
- Edit `.md` files locally, not in Notion's block editor. A file's bytes live in one code block, so a structured edit can split the mirror; the daemon detects this and force-pushes local, but treat `.md` mirror pages as read-only.
- `state.db` is local by design. Don't run two daemons against the same Notion tree from different machines.
- Trashed-block cleanup isn't implemented; Notion auto-purges trash after 30 days.

## License

MIT. See [LICENSE](./LICENSE).

# notion-sync

A background daemon that keeps a local codebase and a Notion page tree in two-way
sync. **The local filesystem is the source of truth** (local-wins). Notion becomes a
mirror you can read, comment on, and lightly edit from anywhere. It's still just a
mirror, though; the files on disk always win.

> v1 scope. Single mapping, UTF-8 text files, local-wins conflicts, folder/file
> mirroring, rename detection, startup reconciliation, NixOS systemd user service.

---

## How it works

```
  local FS  ──(file saved, debounced 1s)──▶  Notion        [watcher + engine]
  local FS  ◀──(page edited, polled 45s)───  Notion        [poller + conflict]

  state.db (SQLite, machine-local) tracks the rel_path ⇄ page-id mapping.
```

- **Folders** become subpages. **Files** become subpages whose body is the file
  content wrapped in syntax-highlighted **code blocks**.
- File contents are **chunked** to respect Notion limits: 2000 UTF-16 units per
  rich-text item (the count Notion enforces, not Rust `char`s), 100 items per code
  block, 100 children per append request. Chunking is purely positional, so
  reassembly is byte-exact.
- **Page title = filename.** Renames are detected by **content hash** and applied
  with `PATCH /v1/pages/{id}` (rename + reparent), so the page keeps its comments
  and history across a `git mv` or refactor instead of being trashed and recreated.
- **Binary / oversized files** get a placeholder page with a ⚠️ warning callout and
  are never written back.
- A shared **token-bucket rate limiter** (~3 req/s) is used by both the watcher and
  poller; 429s honor `Retry-After`, and 409/5xx use exponential backoff + jitter.
- **Files that carry their own code fences** (Markdown, etc.) are mirrored as a
  single code block and should only be edited as raw code or from local. Editing one
  in Notion's block editor can split the mirror into loose blocks; the poller detects
  that, refuses to overwrite local, and re-pushes to repair the page.

## Fidelity gate (run this first)

Write-back is only safe if Notion preserves bytes exactly. Before trusting the
daemon, run the standalone probe against a real workspace:

```
NOTION_TOKEN=secret_xxx cargo run --bin fidelity-probe -- --parent-page-id <PAGE_ID>
```

It writes an adversarial payload (tabs, mixed indentation, trailing whitespace, a
blank line, emoji/CJK/€/é, a >2000-char multibyte run, a final newline) through the
real `/v1/blocks` API, reads it back through the same chunker, and **exits non-zero
if a single byte differs.** If Notion ever mutates content, the chunker must
compensate deterministically before write-back is enabled.

## Build & run

```
cargo build --release
cargo test                      # offline unit/property tests
NOTION_TOKEN=secret_xxx ./target/release/notion-sync --config ./config.toml
```

Live integration test (creates/edits/renames/deletes + a simulated Notion edit):

```
NOTION_TOKEN=secret_xxx NOTION_TEST_PARENT_PAGE_ID=<page id> \
    cargo test --test integration -- --nocapture
```

## Configuration (`config.toml`)

The Notion token is **never** in this file — it comes from `$NOTION_TOKEN`.

```
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

## NixOS systemd user service

`flake.nix` exposes a package (`buildRustPackage`) and a NixOS module. The token is
provided via `EnvironmentFile=` pointing at a `0600` secrets file (or `agenix`/
`sops-nix`), and **never** placed in the Nix store.

```
{
  imports = [ notion-sync.nixosModules.default ];
  services.notion-sync = {
    enable = true;
    configFile = "/home/you/.config/notion-sync/config.toml";
    environmentFile = "/run/secrets/notion-sync.env"; # contains NOTION_TOKEN=...
  };
}
```

The unit runs as a **user service** (so `$HOME`/`$XDG_STATE_HOME` resolve to your
account), restarts on failure, and shuts down gracefully on `SIGTERM`.

## State & limitations

- `state.db` lives under `$XDG_STATE_HOME/notion-sync/` (falls back to
  `~/.local/state/notion-sync/`).
- Conflict backups are written to `<local_root>/.notion-sync/conflicts/`.
- **Multi-machine is out of scope for v1.** `state.db` is intentionally local; do not
  run two daemons against the same Notion tree from different machines.
- Trashed-block cleanup is not implemented — Notion auto-purges trash after 30 days.

## Layout

```
src/
  api/        rate-limited retrying client + serde models + shared token bucket
  chunk.rs    positional chunker/reassembler (fidelity-critical)
  language.rs extension -> Notion code-block language
  hashutil.rs blake3 hashing (change + rename detection)
  config.rs   TOML loader + ignore globs
  state.rs    SQLite state.db
  sync/       engine, watcher, poller, conflict, reconcile, locks, util
  main.rs     daemon entrypoint (reconcile -> watcher + poller, SIGTERM)
  bin/fidelity_probe.rs   the Step-1 fidelity gate
tests/        chunk_roundtrip, state_db, integration (live, credential-gated)
nix/module.nix, flake.nix   NixOS packaging
```

# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.0] - 2026-07-06

### Added
- **`keepWarm` Nix option for relay tunnels.** Tailscale Funnel's ingress goes cold when
  idle: the first request after a quiet stretch 502s while the relay reconnects, which
  silently drops Notion's one-shot webhook delivery (the poller re-pulls it a cycle later)
  and, worse, fails subscription verification, itself a single one-shot POST. The NixOS
  module gained `services.notion-sync.keepWarm` -- an off-by-default system timer that pings
  the public webhook URL on an interval (`45s` default) so the ingress never idles out. Set
  `forcePublicPath = true` for Funnel: a curl from the node takes the direct tailnet path
  and never crosses the ingress, so the timer resolves the funnel name against a public DNS
  resolver (MagicDNS returns the tailnet IP) and hits the edge with `curl --resolve`. Leave
  it off for a cloudflared named tunnel, which holds the connection open. README's "Picking
  a tunnel" documents the failure mode. We hit this deploying against Funnel; the daemon
  needs no change (delivery was already best-effort with a poller fallback), so this is
  deployment glue and lives in the module, not the binary.

## [0.5.0] - 2026-06-09

### Added
- **Webhook receiver (Notion -> local push).** An optional `[webhook]` table starts a
  small HTTP listener that Notion integration webhooks post to, so a remote edit lands in
  seconds instead of waiting for the next poll. Off by default. When enabled it handles
  Notion's one-time verification handshake, persists the signing token under
  `$XDG_STATE_HOME/notion-sync/` so a restart doesn't force re-verification, and verifies
  every event after that (HMAC-SHA256 over the raw body). Notion only delivers to public
  HTTPS, so the intended deployment terminates TLS in a tunnel (e.g. cloudflared) and
  forwards to the loopback listener. A bad bind or a busy port is non-fatal: the daemon
  logs it and keeps polling.
- **Remote-first page discovery.** A page created or edited in Notion that we don't track
  yet is adopted as a local file when it places under a mapping's parent chain and reads
  back as a faithful code body. Both paths feed it: the webhook acts on `page.created` /
  `page.content_updated`, and the poller scans recently-edited untracked pages each cycle
  (bounded per cycle, with foreign pages remembered so it stops re-probing them). An empty
  page, or one split into non-code blocks, is skipped until it gains a real body.

### Changed
- **The poller is a fallback now, not the only path.** With webhooks on, the poll loop
  still runs for the deliveries Notion drops, reorders, or aggregates (delivery is
  at-most-once and best-effort), and it stays the sole mechanism when webhooks are off.
  Echo suppression is shared by both paths: an edit attributed solely to our bot whose
  body still hashes to the last sync is skipped; a human co-author or a diverged body is
  pulled.

### Fixed
- **Discovery matches compact config roots.** A `parent_page_id` written compact (no
  dashes) never matched the dashed UUID the API returns, so every discovered page resolved
  to un-placeable and was silently dropped. Page ids are now normalized (dashes stripped,
  lowercased) before matching a mapping root.

## [0.5.2] - 2026-07-03

### Fixed
- **A missing `local_root` no longer crash-loops the daemon.** Config load used to
  hard-error when a mapping's `local_root` wasn't a directory, so on a late-mount or
  impermanence host a root that isn't there yet at boot took the whole daemon down on a
  systemd restart loop. It now warns and keeps the mapping; reconcile, the poller
  health-check, and the watcher already skip a missing root per mapping, and keeping it in
  the config means the CLI subcommands still resolve that mapping's paths. Trade-off: a
  genuinely typo'd root no longer fails fast, it just warns and never syncs (the warning
  says as much).
- **"No Notion token found" now calls out the `EnvironmentFile` gotcha.** Under systemd the
  `environmentFile` must hold a literal `NOTION_TOKEN=ntn_...` line; a bare token with no
  `NOTION_TOKEN=` is silently ignored and reads as this exact error even though you did
  provide one. The message says so now.
- **Non-code-block pages stop re-warning every poll.** Two paths logged the same warning
  each cycle: discovery re-probed an untracked page split into non-code blocks (it shared
  the `Skipped` outcome with empty pages, which are deliberately re-probed), and a pull
  skipped a tracked split page whose local copy had also diverged without recording that it
  had seen the remote edit. Discovery now separates "no body yet" (re-probe) from "foreign
  blocks" (cache and warn once), and the diverged-pull path records the remote timestamp so
  it warns once per remote change.

## [0.5.1] - 2026-06-10

### Added
- **Wider code-block language detection.** The extension-to-language map now covers most of
  what Notion can highlight (Clojure, Elixir, Elm, Erlang, F#, Fortran, GLSL, Groovy, HCL,
  Julia, Scala, Scheme, Solidity, Protobuf, and ~30 more) instead of falling back to plain
  text, with targets matching Notion's exact picker values. Shared extensions follow GitHub
  Linguist's default: `.m` is Objective-C, `.v` is Verilog, `.pl` stays Perl (Prolog is
  `.pro`). Languages with no real source extension (ASCII Art, BNF, EBNF, Markup, Notion
  Formula) stay manual.

## [0.4.0] - 2026-06-09

### Added
- **Declarative Nix config.** The NixOS module gained a `settings` option that renders
  `config.toml` straight from Nix via `pkgs.formats.toml`, so you no longer hand-place the
  file and point `configFile` at it. `configFile` is now optional and acts as an escape
  hatch; set one or the other. The token still only comes from `environmentFile`, never the
  rendered (world-readable) store path.
- **home-manager module.** `homeManagerModules.default` runs the daemon as a systemd user
  service and materializes config in `$HOME`, so a home-manager-only user can deploy
  notion-sync entirely in Nix, `perDirectory` overrides included.
- **hjem module.** `hjemModules.default` renders the central `config.toml` and per-tree
  `.notion-sync.toml` files for hjem users. Config-only on purpose: hjem doesn't define
  services (feel-co/hjem#63), so the daemon still comes from the NixOS or home-manager
  module.
- **`ns stream`.** Wraps `journalctl --user --unit notion-sync --follow` so you don't have
  to remember it. `-n/--lines` sets the backlog (default 50), `--no-follow` prints and exits.
- **Log presentation flags.** `--color auto|always|never` (auto colors only on a TTY) and
  `--log-time datetime|uptime|none` (datetime carries seconds). Both are exposed as Nix
  module options.

### Changed
- **Logs go to stderr.** Diagnostics no longer mix into stdout, so piped command output
  stays clean.
- **Slimmed the sync internals.** Trimmed dead paths and tightened the engine/poller/
  reconcile ahead of the webhook work, with no behavior change -- the fidelity probe still
  round-trips every byte and the suite stays green.

## [0.3.0] - 2026-06-09

### Added
- **Multiple directory mappings.** The config now accepts repeated `[[mapping]]`
  tables, each with its own `local_root`, `parent_page_id`, and `ignore` list, so one
  daemon mirrors several directories into several Notion parent pages (a single token
  can already see many pages). An optional `name` per mapping labels its subtree; it
  defaults to the final path component of `local_root` and must be unique. A legacy
  single `[mapping]` table still parses unchanged.
- **Per-directory config.** Drop a `.notion-sync.toml` in any mapped directory to extend
  its `ignore` list (additive on top of the central baseline) and override
  `max_file_bytes`, without editing the central config. The file travels with the
  directory, so it can be committed to that repo. Only those two keys are honored;
  registry/secret keys (`parent_page_id`, `local_root`, `token_file`, ...) are rejected so
  a mapping can't be repointed from inside its own tree.
- **`ns config` inspector.** A read-only subcommand that loads the config and prints each
  mapping's resolved `ignore` list and `max_file_bytes` after per-directory overrides merge
  in, so you can eyeball what the daemon will actually do before trusting a sync. No Notion
  calls and no state writes; it just resolves and prints. The token still has to resolve,
  since loading the config does, so point it at a real `token_file` or set `$NOTION_TOKEN`.

### Changed
- **State paths are namespaced per mapping.** Every node/snapshot/journal row is keyed
  as `<mapping>/<path>` so directories with overlapping layouts can't collide. CLI
  subcommands (`log`, `show`, `restore`, ...) now take these `<mapping>/<path>` keys.
- **The mass-delete guard is per mapping.** A missing or empty local tree skips only
  that mapping's reconcile/deletions; the other mappings keep syncing instead of the
  whole pass aborting.
- **Shared object store.** The content-addressed snapshot store moved from
  `<local_root>/.notion-sync/objects` to `$XDG_STATE_HOME/notion-sync/objects` (beside
  `state.db`), so dedup spans every mapping and no single root owns it.

### Fixed
- **The watcher no longer follows symlinks.** A `nix build` drops a `result` symlink into
  the tree; the watcher's `is_dir()` check followed it and mirrored it as a directory
  subpage. It now stats the link itself and skips symlinks, matching what the reconcile
  walk already did.
- **`.notion-sync.toml` and the `.notion-sync/` state dir are always ignored**, regardless
  of the configured `ignore` list, so a config edit can't drag the daemon's own machinery
  into Notion.
  

### Migration
- A pre-0.3 `state.db` is migrated **automatically on first start, but only when exactly
  one mapping is configured**: its rows are re-keyed under that mapping's name and its
  old per-root object store is moved into the shared one. If the config already lists
  several mappings, the daemon refuses with guidance to run once with the original
  single mapping first, then add the rest -- the old rows carry no record of which root
  they belonged to, so splitting them automatically would be a guess.

## [0.2.1] - 2026-06-09

### Changed
- **state.db durability: `synchronous=NORMAL` under WAL.** Dropped the per-commit
  full-database fsync in favor of syncing at checkpoint. This stays crash-safe: the
  database is never corrupted and an application crash (panic, kill, SIGTERM) loses
  nothing. The only tradeoff is that a power loss or hard OS crash can roll back the most
  recent un-checkpointed transactions -- and those are mapping/journal rows the next
  reconcile re-derives by rescanning, so the mirror self-heals. Also sets
  `temp_store=MEMORY` to keep SQLite's transient tables off disk.
- **Under-the-hood performance pass.** Trimmed redundant allocations and passes across
  the chunker/reassembler, the engine's UTF-8/binary classification (one decode instead
  of two), the language-detection lookup, and smaller cleanups in the poller and conflict
  paths. No behavior change: sync output stays byte-for-byte identical (the fidelity probe
  still round-trips every byte).

### Fixed
- **Notion -> local edits land promptly again.** The idle poll backoff was allowed to
  climb to 10 minutes, but remote edits have no push event to wake the daemon, so on a
  long-idle daemon a Notion/AI edit could sit unsynced for minutes while the instant
  local -> Notion watcher push won the race -- the edit appeared to never land or to get
  overwritten. The idle backoff now caps at 90s (was 600s), bounding worst-case
  remote-edit latency with no measurable increase in idle API traffic (one search call
  per cycle). The poller now also logs at `info` when it backs off and when it returns
  to the floor, so a slow pull shows up in the journal instead of looking like a hang.

## [0.2.0] - 2026-06-08

### Added
- **Backup, versioning & recovery.** Content-addressed snapshot store (gzip CAS keyed
  by blake3 with git-style fanout) plus a SQLite sync journal. Every overwrite, pull,
  conflict, and delete captures the prior bytes first, so destructive syncs stay
  reversible.
- **Recovery CLI** (clap): `log`, `history`, `show`, `diff`, `restore`, `backup`,
  `untrash`, and `gc`, with `--at <id|age|RFC3339>` snapshot selection and `--dry-run`.
- `search_pages_by_last_edited` client method backing cheaper poller change-detection.
- `token_file` config option and a friendlier missing-token error.
- Schema migration runner via `PRAGMA user_version` (snapshot + journal tables).

### Changed
- Poller now uses one paginated `/v1/search` per cycle plus adaptive idle backoff
  instead of an O(N) `get_page` scan, with SQL-side filtering of tracked rows.
- Disk I/O routed through `spawn_blocking` / `tokio::fs` off the async runtime.
- README: new "Backup, versioning & recovery" section documenting the CLI surface,
  `--at` formats, gc behavior, and the 30-day untrash window.

### Fixed
- `overwrite_body` appends the new body before trashing old blocks (no transient blank
  page on a mid-operation failure).
- `ensure_placeholder` reuses an existing page when a tracked text file crosses
  `max_file_bytes` instead of orphaning it.
- Trashing a directory removes descendant rows, not just the directory's own row.
- A remotely trashed Notion page (`in_trash`) is treated as a remote-delete (snapshot +
  unlink) instead of blanking the local file.
- `glob_match` handles both-ended `*foo*` contains patterns.
- `list_children` warns instead of silently truncating on `has_more` with no cursor.
- Partial mass-delete guard: reconcile snapshots before trashing and bails on a
  large-but-nonempty deletion.
- `force_push_locked` uses `String::from_utf8` with a `looks_binary` guard.
- Watcher max-wait cap so a continuously-written file still flushes.
- Per-path lock map evicts entries on release.
- fidelity-probe warns instead of silently dropping a failed probe-page trash.
- Periodic health-check surfaces an unmounted `local_root`.
- `state.rs` query helpers bind `query_map` iterators before `collect` (borrow fix).
- **Echo-loop suppression.** The daemon tracks its own pull-writes in a short-TTL
  self-write registry that the watcher consults before pushing, and the poller verifies
  bot-attributed page edits by content hash instead of trusting the most-recent-editor
  field alone. A human edit landing in the same window as one of our writes is no longer
  misattributed, and the pull -> write -> re-push churn that could destabilize a page
  while the daemon runs is eliminated.

### Known gaps
- No token re-auth on a mid-run 401 yet (`token_file` groundwork is in place).

## [0.1.0]

- Initial one-way mirror (local -> Notion) with watcher, poller, reconcile, local-wins
  conflict handling, and the chunk fidelity probe.

[0.6.0]: https://github.com/feltfomo/notion-sync/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/feltfomo/notion-sync/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/feltfomo/notion-sync/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/feltfomo/notion-sync/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/feltfomo/notion-sync/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/feltfomo/notion-sync/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/feltfomo/notion-sync/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/feltfomo/notion-sync/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/feltfomo/notion-sync/releases/tag/v0.1.0

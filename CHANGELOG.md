# Changelog

All notable changes to this project are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.1] - 2026-06-08

### Changed
- **state.db durability: `synchronous=NORMAL` under WAL.** Dropped the per-commit
  full-database fsync in favor of syncing at checkpoint. This stays crash-safe: the
  database is never corrupted and an application crash (panic, kill, SIGTERM) loses
  nothing. The only tradeoff is that a power loss or hard OS crash can roll back the most
  recent un-checkpointed transactions -- and those are mapping/journal rows the next
  reconcile re-derives by rescanning, so the mirror self-heals. Also sets
  `temp_store=MEMORY` to keep SQLite's transient tables off disk.

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

[0.2.1]: https://github.com/feltfomo/notion-sync/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/feltfomo/notion-sync/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/feltfomo/notion-sync/releases/tag/v0.1.0

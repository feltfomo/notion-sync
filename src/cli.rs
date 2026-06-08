//! Command-line interface. The sync daemon (`run`, the default when no subcommand is
//! given) lives in main.rs; this module owns the clap command model plus every
//! non-daemon subcommand: the backup/restore/history surface and snapshot GC.
//!
//! All of these operate on machine-local state (state.db + the content-addressed
//! object store); only `untrash` touches the Notion API. Mutating commands accept
//! `--dry-run`, which logs the intended effect and writes nothing.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing::info;

use crate::state::{rfc3339_minus_secs, SnapshotRow};
use crate::sync::Engine;

#[derive(Debug, Parser)]
#[command(
    name = "notion-sync",
    about = "Two-way sync between a local directory and a Notion page tree",
    version
)]
pub struct Cli {
    /// Path to config.toml (overrides $XDG_CONFIG_HOME/notion-sync/config.toml).
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run the sync daemon (default when no subcommand is given).
    Run,
    /// Print the sync journal: the audit trail of every push/pull/delete.
    Log {
        /// Restrict to a single repo-relative path.
        path: Option<String>,
        /// Maximum number of rows to show.
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// List the snapshots captured for a file, newest first.
    History {
        /// Repo-relative file path.
        path: String,
        #[arg(short, long, default_value_t = 20)]
        limit: usize,
    },
    /// Print a snapshot's contents to stdout.
    Show {
        /// Snapshot id (from `history`).
        #[arg(long)]
        id: Option<i64>,
        /// Repo-relative path (resolved together with --at).
        #[arg(long)]
        path: Option<String>,
        /// Point in time: a snapshot id, an age like 2h/3d/1w, or an RFC3339 timestamp.
        #[arg(long)]
        at: Option<String>,
    },
    /// Diff a snapshot against the current local file.
    Diff {
        /// Repo-relative file path.
        path: String,
        /// Point in time: snapshot id, age (2h/3d), or RFC3339. Default: newest snapshot.
        #[arg(long)]
        at: Option<String>,
    },
    /// Restore a file from a snapshot back onto local disk (local-wins re-pushes it).
    Restore {
        /// Repo-relative file path.
        path: String,
        /// Point in time: snapshot id, age (2h/3d), or RFC3339. Default: newest snapshot.
        #[arg(long)]
        at: Option<String>,
        /// Show what would change without writing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Force a one-off snapshot of the current local copy of a file.
    Backup {
        /// Repo-relative file path.
        path: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Pull a trashed Notion page back out of the trash (while the mapping survives).
    Untrash {
        /// Repo-relative file path.
        path: String,
        #[arg(long)]
        dry_run: bool,
    },
    /// Garbage-collect old snapshot rows and the blobs they no longer reference.
    Gc {
        /// Prune snapshots older than this age (e.g. 30d, 12h).
        #[arg(long, default_value = "30d")]
        older_than: String,
        /// Always keep at least this many snapshots per (path, side).
        #[arg(long, default_value_t = 5)]
        keep_min: u32,
        #[arg(long)]
        dry_run: bool,
    },
}

/// Run a non-daemon subcommand. `Run` is handled by the daemon entrypoint and never
/// reaches here.
pub async fn dispatch(engine: Arc<Engine>, command: Command) -> Result<(), String> {
    match command {
        Command::Run => Err("internal: `run` is handled by the daemon entrypoint".into()),
        Command::Log { path, limit } => log_cmd(&engine, path.as_deref(), limit).await,
        Command::History { path, limit } => history_cmd(&engine, &path, limit).await,
        Command::Show { id, path, at } => {
            show_cmd(&engine, id, path.as_deref(), at.as_deref()).await
        }
        Command::Diff { path, at } => diff_cmd(&engine, &path, at.as_deref()).await,
        Command::Restore { path, at, dry_run } => {
            restore_cmd(&engine, &path, at.as_deref(), dry_run).await
        }
        Command::Backup { path, dry_run } => backup_cmd(&engine, &path, dry_run).await,
        Command::Untrash { path, dry_run } => untrash_cmd(&engine, &path, dry_run).await,
        Command::Gc {
            older_than,
            keep_min,
            dry_run,
        } => gc_cmd(&engine, &older_than, keep_min, dry_run).await,
    }
}

/// Resolve a single snapshot for `path` from an optional `--at` spec:
///   * none           => the newest snapshot,
///   * all-digits     => that snapshot id (must belong to `path`),
///   * <n><s|m|h|d|w> => newest snapshot captured at or before (now - age),
///   * anything else  => treated as an RFC3339 cutoff (nearest at-or-before).
async fn resolve_snapshot(
    engine: &Arc<Engine>,
    path: &str,
    at: Option<&str>,
) -> Result<SnapshotRow, String> {
    let st = engine.state.lock().await;
    match at {
        None => st
            .list_snapshots(path, 1)
            .map_err(|e| e.to_string())?
            .into_iter()
            .next()
            .ok_or_else(|| format!("no snapshots recorded for {path}")),
        Some(spec) if spec.chars().all(|c| c.is_ascii_digit()) && !spec.is_empty() => {
            let id: i64 = spec
                .parse()
                .map_err(|_| format!("bad snapshot id {spec}"))?;
            st.snapshot_by_id(id)
                .map_err(|e| e.to_string())?
                .filter(|s| s.rel_path == path)
                .ok_or_else(|| format!("snapshot {id} not found for {path}"))
        }
        Some(spec) => {
            let cutoff = parse_at_cutoff(spec)?;
            st.snapshot_at_or_before(path, &cutoff)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("no snapshot for {path} at or before {cutoff}"))
        }
    }
}

/// Turn an `--at` spec into an RFC3339 cutoff. An age (2h/3d/1w) is resolved relative
/// to now; anything else is assumed to already be an RFC3339 timestamp.
fn parse_at_cutoff(spec: &str) -> Result<String, String> {
    if let Some(secs) = parse_age_secs(spec) {
        return Ok(rfc3339_minus_secs(secs));
    }
    Ok(spec.to_string())
}

/// Parse `<number><unit>` (s, m, h, d, w) into seconds.
fn parse_age_secs(spec: &str) -> Option<u64> {
    let spec = spec.trim();
    let split = spec.len().checked_sub(1)?;
    let (num, unit) = spec.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    Some(n.saturating_mul(mult))
}

fn short(h: &Option<String>) -> String {
    match h {
        Some(s) if s.len() >= 8 => s[..8].to_string(),
        Some(s) => s.clone(),
        None => "-".to_string(),
    }
}

async fn log_cmd(engine: &Arc<Engine>, path: Option<&str>, limit: usize) -> Result<(), String> {
    let rows = {
        let st = engine.state.lock().await;
        st.list_journal(path, limit).map_err(|e| e.to_string())?
    };
    if rows.is_empty() {
        println!("(no journal entries)");
        return Ok(());
    }
    for r in rows {
        println!(
            "{ts}  {action:<14} {side:<11} {path}  {from}->{to}",
            ts = r.ts,
            action = r.action,
            side = r.side,
            path = r.rel_path,
            from = short(&r.from_hash),
            to = short(&r.to_hash),
        );
    }
    Ok(())
}

async fn history_cmd(engine: &Arc<Engine>, path: &str, limit: usize) -> Result<(), String> {
    let rows = {
        let st = engine.state.lock().await;
        st.list_snapshots(path, limit).map_err(|e| e.to_string())?
    };
    if rows.is_empty() {
        println!("(no snapshots for {path})");
        return Ok(());
    }
    println!(
        "{:>6}  {:<24} {:<7} {:<12} {:>9}  hash",
        "id", "captured_at", "side", "reason", "bytes"
    );
    for s in rows {
        println!(
            "{:>6}  {:<24} {:<7} {:<12} {:>9}  {}",
            s.id,
            s.captured_at,
            s.side,
            s.reason,
            s.size_bytes,
            short(&Some(s.blake3)),
        );
    }
    Ok(())
}

async fn show_cmd(
    engine: &Arc<Engine>,
    id: Option<i64>,
    path: Option<&str>,
    at: Option<&str>,
) -> Result<(), String> {
    let snap = match (id, path) {
        (Some(id), _) => {
            let st = engine.state.lock().await;
            st.snapshot_by_id(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("snapshot {id} not found"))?
        }
        (None, Some(p)) => resolve_snapshot(engine, p, at).await?,
        (None, None) => return Err("show needs either --id or --path".into()),
    };
    let bytes = engine
        .store
        .get(&snap.blake3)
        .await
        .map_err(|e| e.to_string())?;
    print!("{}", String::from_utf8_lossy(&bytes));
    Ok(())
}

async fn diff_cmd(engine: &Arc<Engine>, path: &str, at: Option<&str>) -> Result<(), String> {
    let snap = resolve_snapshot(engine, path, at).await?;
    let old_bytes = engine
        .store
        .get(&snap.blake3)
        .await
        .map_err(|e| e.to_string())?;
    let abs = engine.abs_path(path);
    let new_bytes = tokio::fs::read(&abs).await.unwrap_or_default();
    let old = String::from_utf8_lossy(&old_bytes);
    let new = String::from_utf8_lossy(&new_bytes);
    if old == new {
        println!(
            "no differences (snapshot {} matches current {path})",
            snap.id
        );
        return Ok(());
    }
    println!(
        "--- snapshot {} ({}, {})",
        snap.id, snap.captured_at, snap.side
    );
    println!("+++ local {path}");
    print_block_diff(&old, &new);
    Ok(())
}

/// Dependency-free block diff: trim the common prefix and suffix, then print the
/// differing middle as `-old` / `+new`. Not a minimal-edit diff, but correct and
/// enough to eyeball what a snapshot would change.
fn print_block_diff(old: &str, new: &str) {
    let o: Vec<&str> = old.lines().collect();
    let n: Vec<&str> = new.lines().collect();
    let mut start = 0;
    while start < o.len() && start < n.len() && o[start] == n[start] {
        start += 1;
    }
    let mut end = 0;
    while end < o.len() - start
        && end < n.len() - start
        && o[o.len() - 1 - end] == n[n.len() - 1 - end]
    {
        end += 1;
    }
    for line in &o[start..o.len() - end] {
        println!("-{line}");
    }
    for line in &n[start..n.len() - end] {
        println!("+{line}");
    }
}

async fn restore_cmd(
    engine: &Arc<Engine>,
    path: &str,
    at: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    let snap = resolve_snapshot(engine, path, at).await?;
    let bytes = engine
        .store
        .get(&snap.blake3)
        .await
        .map_err(|e| e.to_string())?;
    let abs = engine.abs_path(path);
    if dry_run {
        println!(
            "[dry-run] would restore {path} from snapshot {} ({}, {} bytes) -> {}",
            snap.id,
            snap.captured_at,
            snap.size_bytes,
            abs.display()
        );
        return Ok(());
    }
    // Snapshot the current local copy first so the restore is itself reversible.
    if let Ok(cur) = tokio::fs::read(&abs).await {
        engine
            .capture(path, None, "local", "pre-restore", cur)
            .await;
    }
    if let Some(parent) = abs.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    // Atomic publish (temp + rename), dependency-free.
    let tmp = abs.with_extension("notion-sync.restore.tmp");
    tokio::fs::write(&tmp, &bytes)
        .await
        .map_err(|e| e.to_string())?;
    tokio::fs::rename(&tmp, &abs)
        .await
        .map_err(|e| e.to_string())?;
    info!(path, snapshot = snap.id, "restored file from snapshot");
    println!(
        "restored {path} from snapshot {} ({})",
        snap.id, snap.captured_at
    );
    println!("local-wins: the daemon (or next `run`) will push this back to Notion.");
    Ok(())
}

async fn backup_cmd(engine: &Arc<Engine>, path: &str, dry_run: bool) -> Result<(), String> {
    let abs = engine.abs_path(path);
    let bytes = tokio::fs::read(&abs)
        .await
        .map_err(|e| format!("cannot read {}: {e}", abs.display()))?;
    if dry_run {
        println!("[dry-run] would snapshot {path} ({} bytes)", bytes.len());
        return Ok(());
    }
    let page_id = {
        let st = engine.state.lock().await;
        st.get_by_path(path)
            .ok()
            .flatten()
            .map(|n| n.notion_page_id)
    };
    match engine
        .capture(path, page_id.as_deref(), "local", "manual", bytes)
        .await
    {
        Some(h) => println!("snapshot saved for {path} ({})", short(&Some(h))),
        None => return Err("snapshot failed (see logs)".into()),
    }
    Ok(())
}

async fn untrash_cmd(engine: &Arc<Engine>, path: &str, dry_run: bool) -> Result<(), String> {
    let node = {
        let st = engine.state.lock().await;
        st.get_by_path(path).map_err(|e| e.to_string())?
    };
    let Some(node) = node else {
        return Err(format!(
            "{path} is not tracked, so its page id is unknown; cannot untrash (v1 keeps no \
             mapping for fully-deleted nodes)"
        ));
    };
    if dry_run {
        println!(
            "[dry-run] would untrash Notion page {} for {path}",
            node.notion_page_id
        );
        return Ok(());
    }
    engine
        .api
        .update_page(&node.notion_page_id, None, None, Some(false))
        .await
        .map_err(|e| e.to_string())?;
    info!(path, page = %node.notion_page_id, "untrashed page");
    println!("untrashed {path} (page {})", node.notion_page_id);
    Ok(())
}

async fn gc_cmd(
    engine: &Arc<Engine>,
    older_than: &str,
    keep_min: u32,
    dry_run: bool,
) -> Result<(), String> {
    let secs = parse_age_secs(older_than)
        .ok_or_else(|| format!("bad --older-than {older_than} (use forms like 7d, 12h)"))?;
    let cutoff = rfc3339_minus_secs(secs);
    if dry_run {
        let referenced = {
            let st = engine.state.lock().await;
            st.distinct_snapshot_hashes()
                .map_err(|e| e.to_string())?
                .len()
        };
        println!(
            "[dry-run] would prune snapshot rows older than {cutoff} (keep_min={keep_min} per \
             path/side); {referenced} distinct blobs are currently referenced"
        );
        return Ok(());
    }
    let removed_rows = {
        let mut st = engine.state.lock().await;
        st.gc_snapshots(&cutoff, keep_min)
            .map_err(|e| e.to_string())?
    };
    let keep = {
        let st = engine.state.lock().await;
        st.distinct_snapshot_hashes().map_err(|e| e.to_string())?
    };
    let store = engine.store.clone();
    let (objs, freed) = tokio::task::spawn_blocking(move || store.gc_blocking(&keep))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    info!(removed_rows, objs, freed, "gc complete");
    println!("gc: pruned {removed_rows} snapshot rows, deleted {objs} blobs, freed {freed} bytes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_parsing_units() {
        assert_eq!(parse_age_secs("30s"), Some(30));
        assert_eq!(parse_age_secs("5m"), Some(300));
        assert_eq!(parse_age_secs("2h"), Some(7_200));
        assert_eq!(parse_age_secs("3d"), Some(259_200));
        assert_eq!(parse_age_secs("1w"), Some(604_800));
    }

    #[test]
    fn age_parsing_rejects_junk() {
        assert_eq!(parse_age_secs("abc"), None);
        assert_eq!(parse_age_secs("10y"), None); // unsupported unit
        assert_eq!(parse_age_secs(""), None);
        assert_eq!(parse_age_secs("h"), None); // missing number
    }

    #[test]
    fn at_cutoff_passes_through_rfc3339() {
        let ts = "2026-01-02T03:04:05.000Z";
        assert_eq!(parse_at_cutoff(ts).unwrap(), ts);
    }

    #[test]
    fn at_cutoff_resolves_age_to_timestamp() {
        let out = parse_at_cutoff("1d").unwrap();
        assert_ne!(out, "1d");
        assert!(out.contains('T') && out.ends_with('Z'));
    }

    #[test]
    fn short_truncates_to_eight() {
        assert_eq!(short(&Some("0123456789".to_string())), "01234567");
        assert_eq!(short(&Some("abc".to_string())), "abc");
        assert_eq!(short(&None), "-");
    }
}

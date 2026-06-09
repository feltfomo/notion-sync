//! Local filesystem watcher with debounce. Emits coalesced sync events to the engine.
//!
//! `notify` delivers raw OS events on its own thread; we forward them into an async
//! channel and debounce per-path: a node is only synced after `debounce_ms` of quiet.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use super::engine::Engine;
use super::util;
use crate::config::is_ignored;

pub async fn run(engine: Arc<Engine>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<PathBuf>();
    // Snapshot the roots once so the event loop can map an absolute path back to its
    // mapping (and apply that mapping's ignore patterns) without locking the engine on
    // every filesystem event.
    let roots: Vec<(String, PathBuf, Vec<String>)> = engine
        .cfg
        .mappings
        .iter()
        .map(|m| (m.name.clone(), m.local_root.clone(), m.ignore.clone()))
        .collect();

    // notify watcher runs on its own thread and pushes absolute paths.
    let tx2 = raw_tx.clone();
    let mut watcher = match notify::recommended_watcher(move |res: notify::Result<Event>| match res
    {
        Ok(event) => {
            if matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            ) {
                for path in event.paths {
                    let _ = tx2.send(path);
                }
            }
        }
        Err(e) => error!(error = %e, "watch error"),
    }) {
        Ok(w) => w,
        Err(e) => {
            error!(error = %e, "failed to create watcher");
            return;
        }
    };
    // Watch every mapping root. A single unwatchable root (bad permissions, not yet
    // mounted) is logged and skipped rather than killing the whole watcher; we only
    // bail if not one root could be watched.
    let mut watched = 0usize;
    for (name, root, _) in &roots {
        match watcher.watch(root, RecursiveMode::Recursive) {
            Ok(()) => {
                info!(mapping = %name, root = %root.display(), "watching local tree");
                watched += 1;
            }
            Err(e) => {
                error!(mapping = %name, root = %root.display(), error = %e, "failed to start recursive watch")
            }
        }
    }
    if watched == 0 {
        error!("no local roots could be watched; watcher exiting");
        return;
    }

    let debounce = Duration::from_millis(engine.cfg.debounce_ms);
    // Max-wait cap: a continuously-written file keeps pushing its debounce deadline out
    // forever and would never sync. Fire at the earlier of (last_event + debounce) and
    // (first_event + MAX_WAIT_MULT * debounce) so steady writers still flush periodically.
    const MAX_WAIT_MULT: u32 = 10;
    let max_wait = debounce.saturating_mul(MAX_WAIT_MULT);
    // rel -> (soft deadline from last event, hard cap from first event)
    let mut pending: HashMap<String, (Instant, Instant)> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(150));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { info!("watcher shutting down"); break; }
            }
            Some(abs) = raw_rx.recv() => {
                // to_rel resolves which mapping the path belongs to, applies that
                // mapping's ignore patterns, and returns the namespaced rel_path.
                if let Some(rel) = to_rel(&roots, &abs) {
                    debug!(rel, "debouncing event");
                    let now = Instant::now();
                    pending
                        .entry(rel)
                        .and_modify(|(deadline, _cap)| *deadline = now + debounce)
                        .or_insert((now + debounce, now + max_wait));
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let ready: Vec<String> = pending
                    .iter()
                    .filter(|(_, (deadline, cap))| *deadline <= now || *cap <= now)
                    .map(|(p, _)| p.clone())
                    .collect();
                for rel in ready {
                    pending.remove(&rel);
                    dispatch(&engine, &rel).await;
                }
            }
        }
    }
}

async fn dispatch(engine: &Arc<Engine>, rel: &str) {
    let abs = engine.abs_path(rel);
    let result = if !abs.exists() {
        engine.handle_delete(rel).await
    } else if abs.is_dir() {
        engine.ensure_dir(rel).await
    } else {
        engine.sync_file(rel).await
    };
    if let Err(e) = result {
        warn!(rel, error = %e, "sync failed");
    }
}

/// Map an absolute filesystem path to its namespaced rel_path (`<mapping>/<within>`),
/// or `None` if it belongs to no watched root or its mapping ignores it. The
/// longest-matching root wins so nested mapping roots resolve to the most specific one.
fn to_rel(roots: &[(String, PathBuf, Vec<String>)], abs: &std::path::Path) -> Option<String> {
    let mut best: Option<(usize, &str, String, &Vec<String>)> = None;
    for (name, root, ignore) in roots {
        if let Ok(within) = abs.strip_prefix(root) {
            let within_rel = util::rel_to_unix(within);
            if within_rel.is_empty() {
                continue; // the root directory itself, never a node
            }
            let depth = root.components().count();
            if best.as_ref().map(|(d, ..)| depth > *d).unwrap_or(true) {
                best = Some((depth, name, within_rel, ignore));
            }
        }
    }
    let (_, name, within_rel, ignore) = best?;
    if is_ignored(std::path::Path::new(&within_rel), ignore) {
        return None;
    }
    Some(format!("{name}/{within_rel}"))
}

#[cfg(test)]
mod tests {
    use super::to_rel;
    use std::path::{Path, PathBuf};

    fn roots() -> Vec<(String, PathBuf, Vec<String>)> {
        vec![
            (
                "app".to_string(),
                PathBuf::from("/home/me/app"),
                vec!["target".to_string(), ".git".to_string()],
            ),
            ("docs".to_string(), PathBuf::from("/home/me/docs"), vec![]),
        ]
    }

    #[test]
    fn maps_into_the_owning_mapping_namespace() {
        let r = roots();
        assert_eq!(
            to_rel(&r, Path::new("/home/me/app/src/main.rs")).as_deref(),
            Some("app/src/main.rs")
        );
        assert_eq!(
            to_rel(&r, Path::new("/home/me/docs/readme.md")).as_deref(),
            Some("docs/readme.md")
        );
    }

    #[test]
    fn root_itself_and_unknown_paths_are_none() {
        let r = roots();
        assert_eq!(to_rel(&r, Path::new("/home/me/app")), None);
        assert_eq!(to_rel(&r, Path::new("/somewhere/else/x.rs")), None);
    }

    #[test]
    fn applies_the_owning_mappings_ignore_patterns() {
        let r = roots();
        assert_eq!(to_rel(&r, Path::new("/home/me/app/target/debug/x")), None);
        // docs has no ignore list, so a same-named dir there is still synced.
        assert_eq!(
            to_rel(&r, Path::new("/home/me/docs/target/notes.md")).as_deref(),
            Some("docs/target/notes.md")
        );
    }
}

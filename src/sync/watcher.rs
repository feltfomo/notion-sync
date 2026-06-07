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
    let root = engine.cfg.local_root.clone();

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
    if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
        error!(error = %e, "failed to start recursive watch");
        return;
    }
    info!(root = %root.display(), "watching local tree");

    let debounce = Duration::from_millis(engine.cfg.debounce_ms);
    let mut pending: HashMap<String, Instant> = HashMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(150));

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { info!("watcher shutting down"); break; }
            }
            Some(abs) = raw_rx.recv() => {
                if let Some(rel) = to_rel(&root, &abs) {
                    if rel.is_empty() || is_ignored(std::path::Path::new(&rel), &engine.cfg.ignore) {
                        continue;
                    }
                    debug!(rel, "debouncing event");
                    pending.insert(rel, Instant::now() + debounce);
                }
            }
            _ = tick.tick() => {
                let now = Instant::now();
                let ready: Vec<String> = pending
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
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

fn to_rel(root: &std::path::Path, abs: &std::path::Path) -> Option<String> {
    abs.strip_prefix(root).ok().map(util::rel_to_unix)
}

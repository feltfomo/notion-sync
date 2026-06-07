//! Notion poller. Every `poll_interval_secs`, scan tracked file pages for changes
//! and pull them down. Echo-loop suppression: skip pages whose most recent edit was
//! made by our own integration bot (we caused that edit ourselves).

use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::engine::Engine;

pub async fn run(engine: Arc<Engine>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let interval = Duration::from_secs(engine.cfg.poll_interval_secs);
    let mut tick = tokio::time::interval(interval);
    info!(secs = engine.cfg.poll_interval_secs, "poller started");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { info!("poller shutting down"); break; }
            }
            _ = tick.tick() => {
                if let Err(e) = poll_once(&engine).await {
                    warn!(error = %e, "poll cycle failed");
                }
            }
        }
    }
}

async fn poll_once(engine: &Arc<Engine>) -> Result<(), String> {
    let nodes = { engine.state.lock().await.all_tracked().map_err(|e| e.to_string())? };
    for node in nodes {
        if node.kind != crate::state::NodeKind::File || node.is_binary_placeholder {
            continue;
        }
        // Cheap freshness check via page metadata.
        let page = match engine.api.get_page(&node.notion_page_id).await {
            Ok(p) => p,
            Err(e) => {
                warn!(rel_path = %node.rel_path, error = %e, "failed to fetch page metadata");
                continue;
            }
        };
        // Unchanged since last sync?
        if node.notion_last_edited.as_deref() == Some(page.last_edited_time.as_str()) {
            continue;
        }
        // Echo suppression: the latest edit is ours.
        if page
            .last_edited_by
            .as_ref()
            .map(|u| u.id == engine.bot_user_id)
            .unwrap_or(false)
        {
            debug!(rel_path = %node.rel_path, "skipping self-authored edit (echo)");
            // Still record the new timestamp so we don't re-evaluate it forever.
            let mut updated = node.clone();
            updated.notion_last_edited = Some(page.last_edited_time);
            let _ = engine.state.lock().await.upsert(&updated);
            continue;
        }
        info!(rel_path = %node.rel_path, "detected external Notion edit; pulling");
        if let Err(e) = engine.pull_page(&node).await {
            warn!(rel_path = %node.rel_path, error = %e, "pull failed");
        }
    }
    Ok(())
}

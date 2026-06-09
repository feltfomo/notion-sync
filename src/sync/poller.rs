//! Notion poller. Periodically scans tracked file pages for external changes and
//! pulls them down. Three efficiency/robustness measures layer on top of the basic
//! scan:
//!
//! * #8 one paginated POST /v1/search per cycle yields last-edit timestamps for many
//!   pages at once, so unchanged nodes are skipped WITHOUT an individual GET
//!   /v1/pages call (the old behavior was O(N) GETs every cycle).
//! * idle backoff: quiet cycles stretch the interval up to a cap; any detected change
//!   snaps it back to the configured floor.
//! * #17 periodic root health-check: confirm the configured parent page is still
//!   reachable so a revoked share / trashed root surfaces loudly.
//!
//! Echo-loop suppression skips pages whose latest edit was authored by our own
//! integration bot AND whose content still matches what we last synced; a bot-attributed
//! page whose body has actually diverged (a human edit in the same window) is still pulled.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::engine::Engine;
use crate::hashutil;

/// Run the root health-check once every this many poll cycles.
const HEALTH_CHECK_EVERY: u32 = 20;

pub async fn run(engine: Arc<Engine>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let floor = Duration::from_secs(engine.cfg.poll_interval_secs.max(1));
    // Cap idle backoff at 16x the floor, or 10 minutes, whichever is smaller.
    let ceil = std::cmp::min(floor.saturating_mul(16), Duration::from_secs(600));
    let mut delay = floor;
    let mut cycle: u32 = 0;
    info!(secs = engine.cfg.poll_interval_secs, "poller started");

    loop {
        let sleep = tokio::time::sleep(delay);
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() { info!("poller shutting down"); break; }
            }
            _ = sleep => {
                cycle = cycle.wrapping_add(1);
                if cycle % HEALTH_CHECK_EVERY == 0 {
                    health_check(&engine).await;
                }
                match poll_once(&engine).await {
                    Ok(changed) => {
                        if changed > 0 {
                            delay = floor; // activity: keep polling eagerly
                        } else {
                            delay = std::cmp::min(delay.saturating_mul(2), ceil);
                            debug!(next_secs = delay.as_secs(), "idle cycle; backing off");
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "poll cycle failed");
                        delay = floor; // retry promptly after an error
                    }
                }
            }
        }
    }
}

/// Returns the number of nodes that changed (pulled or remotely deleted) this cycle
/// so the caller can drive idle backoff.
async fn poll_once(engine: &Arc<Engine>) -> Result<usize, String> {
    let nodes = {
        engine
            .state
            .lock()
            .await
            .all_tracked()
            .map_err(|e| e.to_string())?
    };

    // #8: one paginated search gives last-edit timestamps for many pages, letting us
    // skip unchanged nodes without a per-node GET. Pages absent from the map (huge
    // workspaces, pagination cap, search lag) fall back to an individual fetch.
    let recent: HashMap<String, String> = match engine.api.search_pages_by_last_edited().await {
        Ok(pairs) => pairs.into_iter().collect(),
        Err(e) => {
            warn!(error = %e, "search prefilter failed; falling back to per-node fetch");
            HashMap::new()
        }
    };

    let mut changed = 0usize;
    for node in nodes {
        if node.kind != crate::state::NodeKind::File || node.is_binary_placeholder {
            continue;
        }

        // Fast path: search already reported this page's last-edit time and it matches
        // what we last synced -> nothing to do, no GET required.
        if let Some(ts) = recent.get(&node.notion_page_id) {
            if node.notion_last_edited.as_deref() == Some(ts.as_str()) {
                continue;
            }
        }

        // Authoritative metadata (needed for trashed() + last_edited_by anyway).
        let page = match engine.api.get_page(&node.notion_page_id).await {
            Ok(p) => p,
            Err(e) => {
                warn!(rel_path = %node.rel_path, error = %e, "failed to fetch page metadata");
                continue;
            }
        };

        // #4: a trashed remote page is a remote-delete signal. Mirror it locally
        // (snapshot + unlink + drop rows) rather than pulling a body that's gone.
        if page.trashed() {
            info!(rel_path = %node.rel_path, "remote page trashed; applying remote delete locally");
            if let Err(e) = engine.handle_remote_delete(&node).await {
                warn!(rel_path = %node.rel_path, error = %e, "remote delete failed");
            } else {
                changed += 1;
            }
            continue;
        }

        // Unchanged since last sync?
        if node.notion_last_edited.as_deref() == Some(page.last_edited_time.as_str()) {
            continue;
        }

        // Echo suppression: the latest edit looks like ours (bot-authored).
        //
        // `last_edited_by` reports only the *most recent* editor, so a human edit that
        // lands in the same window as one of our own writes is attributed to the bot.
        // Trusting it alone silently drops that human edit. So when a page looks
        // bot-authored we VERIFY by content: reassemble the remote body and compare its
        // hash to what we last synced. Identical content is a true echo (skip); any
        // divergence is a real edit we must still pull. A body we can't read cleanly
        // (unreadable, or split into foreign blocks) is never treated as an echo -- we
        // fall through so resolve_pull can handle or repair it.
        let looks_self_authored = page
            .last_edited_by
            .as_ref()
            .map(|u| u.id == engine.bot_user_id)
            .unwrap_or(false);
        if looks_self_authored {
            let is_echo = match engine.read_page_body(&node).await {
                Ok(body) if body.foreign_blocks == 0 => {
                    node.content_hash.as_deref() == Some(hashutil::hash_str(&body.text).as_str())
                }
                _ => false,
            };
            if is_echo {
                debug!(rel_path = %node.rel_path, "skipping self-authored edit (content matches last sync)");
                // Record the new timestamp so we don't re-evaluate it forever.
                let mut updated = node.clone();
                updated.notion_last_edited = Some(page.last_edited_time);
                let _ = engine.state.lock().await.upsert(&updated);
                continue;
            }
            debug!(rel_path = %node.rel_path, "bot-authored edit but content diverged from last sync; pulling");
        }

        info!(rel_path = %node.rel_path, "detected external Notion edit; pulling");
        if let Err(e) = engine.pull_page(&node).await {
            warn!(rel_path = %node.rel_path, error = %e, "pull failed");
        } else {
            changed += 1;
        }
    }
    Ok(changed)
}

/// #17: confirm the configured root page is still reachable; a revoked share or a
/// trashed root otherwise manifests only as confusing per-file failures.
async fn health_check(engine: &Arc<Engine>) {
    // Local side: an unmount or impermanence wipe leaves local_root gone. Surface it
    // once here instead of as a storm of per-file ENOENT errors during sync (#17 local).
    if !engine.cfg.local_root.is_dir() {
        warn!(
            root = %engine.cfg.local_root.display(),
            "root health-check: local_root is missing or not a directory"
        );
    }
    match engine.api.get_page(&engine.cfg.parent_page_id).await {
        Ok(p) if p.trashed() => warn!(
            parent = %engine.cfg.parent_page_id,
            "root health-check: configured parent page is in trash"
        ),
        Ok(_) => debug!("root health-check ok"),
        Err(e) => {
            warn!(parent = %engine.cfg.parent_page_id, error = %e, "root health-check failed")
        }
    }
}

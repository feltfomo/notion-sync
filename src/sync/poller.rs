//! Notion poller. Periodically scans tracked file pages for external changes and
//! pulls them down. Three measures layer on top of the basic scan:
//!
//! * one paginated POST /v1/search per cycle yields last-edit timestamps for many
//!   pages at once, so unchanged nodes are skipped without an individual GET /v1/pages.
//! * idle backoff: quiet cycles stretch the interval up to a cap; any detected change
//!   snaps it back to the configured floor.
//! * periodic root health-check: confirm the configured parent page is still reachable
//!   so a revoked share / trashed root surfaces loudly.
//!
//! Echo-loop suppression skips pages whose latest edit was authored by our own
//! integration bot AND whose content still matches what we last synced; a bot-attributed
//! page whose body has actually diverged is still pulled.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, info, warn};

use super::engine::{Discovery, Engine};

/// Run the root health-check once every this many poll cycles.
const HEALTH_CHECK_EVERY: u32 = 20;

/// Hard ceiling on the idle-poll backoff, in seconds.
///
/// Remote (Notion) edits have no push-style event to wake us; the only way we notice
/// one is the next poll, so the idle backoff is felt directly as Notion->local sync
/// latency. The idle API savings past a minute or two are negligible (one search call
/// per cycle), so cap the backoff near the floor to keep remote edits landing promptly.
const MAX_IDLE_BACKOFF_SECS: u64 = 90;

/// Cap on discovery probes per poll cycle. Each probe is a GET, and a freshly enabled
/// daemon facing a big workspace shouldn't turn one cycle into a scan of every recently
/// edited foreign page; the rest are picked up over subsequent cycles.
const MAX_DISCOVERY_PROBES_PER_CYCLE: usize = 20;

/// Ceiling for the idle backoff given the configured poll floor. Never below the floor,
/// so a deployment that deliberately polls slowly is still honored.
fn idle_backoff_ceiling(floor: Duration) -> Duration {
    std::cmp::max(floor, Duration::from_secs(MAX_IDLE_BACKOFF_SECS))
}

/// Next delay after a quiet cycle: double, but never past the ceiling.
fn next_idle_delay(delay: Duration, ceiling: Duration) -> Duration {
    std::cmp::min(delay.saturating_mul(2), ceiling)
}

pub async fn run(engine: Arc<Engine>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
    let floor = Duration::from_secs(engine.cfg.poll_interval_secs.max(1));
    let ceil = idle_backoff_ceiling(floor);
    let mut delay = floor;
    let mut cycle: u32 = 0;
    // Page ids ruled out as not part of any mapping tree, remembered for this run so the
    // untracked-page discovery scan doesn't re-probe the same foreign pages every cycle.
    let mut not_ours: HashSet<String> = HashSet::new();
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
                match poll_once(&engine, &mut not_ours).await {
                    Ok(changed) => {
                        if changed > 0 {
                            // Activity: poll eagerly again, announcing the return to the floor.
                            if delay > floor {
                                info!(
                                    interval_secs = floor.as_secs(),
                                    "Notion activity; poll interval back to floor"
                                );
                            }
                            delay = floor;
                        } else {
                            let previous = delay;
                            delay = next_idle_delay(delay, ceil);
                            // Log only the step up, not every quiet cycle: once the tree goes
                            // quiet the interval grows, so a Notion edit can take up to this long.
                            if delay > previous {
                                info!(
                                    interval_secs = delay.as_secs(),
                                    "no recent activity; backing off, Notion edits may take up to this long to land"
                                );
                            }
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
async fn poll_once(engine: &Arc<Engine>, not_ours: &mut HashSet<String>) -> Result<usize, String> {
    let nodes = {
        engine
            .state
            .lock()
            .await
            .tracked_files()
            .map_err(|e| e.to_string())?
    };

    // One paginated search gives last-edit timestamps for many pages, letting us skip
    // unchanged nodes without a per-node GET. Pages absent from the map (huge
    // workspaces, pagination cap, search lag) fall back to an individual fetch. The same
    // list (newest first) also drives the untracked-page discovery scan below.
    let recent_pairs = match engine.api.search_pages_by_last_edited().await {
        Ok(pairs) => pairs,
        Err(e) => {
            warn!(error = %e, "search prefilter failed; falling back to per-node fetch");
            Vec::new()
        }
    };
    let recent: HashMap<String, String> = recent_pairs.iter().cloned().collect();

    let mut changed = 0usize;
    for node in nodes {
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

        // A trashed remote page is a remote-delete signal. Mirror it locally
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
        // `last_edited_by` reports only the most recent editor, so a human edit landing
        // in the same window as one of our writes is attributed to the bot. So when a
        // page looks bot-authored, VERIFY by content: reassemble the remote body and
        // compare its hash to what we last synced. Identical is a true echo (skip); any
        // divergence is a real edit to pull. A body we can't read cleanly is never an
        // echo -- fall through so resolve_pull can handle or repair it.
        let looks_self_authored = page
            .last_edited_by
            .as_ref()
            .map(|u| u.id == engine.bot_user_id)
            .unwrap_or(false);
        if looks_self_authored {
            // The content check lives on the engine so the webhook worker can apply the
            // same rule to its event `authors`.
            if engine.remote_body_matches_last_sync(&node).await {
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

    // Fallback discovery: catch pages created or edited in Notion that we don't track yet
    // (webhooks off, or a page.created we never received). Bounded per cycle because each
    // probe costs a GET, and foreign pages are remembered so we stop asking.
    changed += discover_untracked(engine, &recent_pairs, not_ours).await;

    Ok(changed)
}

/// Probe up to a capped number of recently-edited, untracked pages for discovery,
/// returning how many became new local files. Foreign (un-placeable) pages are cached in
/// `not_ours` so later cycles skip them; an "ours but no body yet" page is left uncached
/// so it's re-probed once it gains content.
async fn discover_untracked(
    engine: &Arc<Engine>,
    recent_newest_first: &[(String, String)],
    not_ours: &mut HashSet<String>,
) -> usize {
    let tracked_ids: HashSet<String> = match engine.state.lock().await.all_tracked() {
        Ok(nodes) => nodes.into_iter().map(|n| n.notion_page_id).collect(),
        Err(e) => {
            warn!(error = %e, "could not load tracked ids for discovery scan; skipping it this cycle");
            return 0;
        }
    };
    let candidates = discovery_candidates(
        recent_newest_first,
        &tracked_ids,
        not_ours,
        MAX_DISCOVERY_PROBES_PER_CYCLE,
    );
    let mut created = 0usize;
    for page_id in candidates {
        match engine.discover_remote_page(page_id).await {
            Ok(Discovery::Created) => {
                info!(page = %page_id, "poller discovered untracked Notion page");
                created += 1;
            }
            Ok(Discovery::NotPlaceable) => {
                not_ours.insert(page_id.to_string());
            }
            // AlreadyTracked / Skipped: don't cache -- a skipped page may gain a body, and
            // a now-tracked one drops out of the candidate set on its own next cycle.
            Ok(_) => {}
            Err(e) => warn!(page = %page_id, error = %e, "discovery probe failed"),
        }
    }
    created
}

/// Pure candidate selection: recently-edited pages (newest first) we neither track nor
/// have already ruled out, capped to bound the GETs one cycle can issue.
fn discovery_candidates<'a>(
    recent_newest_first: &'a [(String, String)],
    tracked_ids: &HashSet<String>,
    not_ours: &HashSet<String>,
    cap: usize,
) -> Vec<&'a str> {
    recent_newest_first
        .iter()
        .map(|(id, _)| id.as_str())
        .filter(|id| !tracked_ids.contains(*id) && !not_ours.contains(*id))
        .take(cap)
        .collect()
}

/// Confirm the configured root page is still reachable; a revoked share or a trashed
/// root otherwise manifests only as confusing per-file failures.
async fn health_check(engine: &Arc<Engine>) {
    // One mapping at a time: a single revoked share or unmounted root should surface as
    // its own clearly-labeled warning, not hide behind the first mapping that happens
    // to be healthy.
    for m in &engine.cfg.mappings {
        // Local side: an unmount or impermanence wipe leaves a root gone. Surface it
        // once here instead of as a storm of per-file ENOENT errors during sync.
        if !m.local_root.is_dir() {
            warn!(
                mapping = %m.name, root = %m.local_root.display(),
                "root health-check: local_root is missing or not a directory"
            );
        }
        match engine.api.get_page(&m.parent_page_id).await {
            Ok(p) if p.trashed() => warn!(
                mapping = %m.name, parent = %m.parent_page_id,
                "root health-check: configured parent page is in trash"
            ),
            Ok(_) => debug!(mapping = %m.name, "root health-check ok"),
            Err(e) => {
                warn!(mapping = %m.name, parent = %m.parent_page_id, error = %e, "root health-check failed")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovery_candidates_skips_tracked_and_known_foreign_and_caps() {
        let recent: Vec<(String, String)> = vec![
            ("a".into(), "t3".into()),
            ("b".into(), "t2".into()),
            ("c".into(), "t1".into()),
            ("d".into(), "t0".into()),
        ];
        let tracked: HashSet<String> = ["b".to_string()].into_iter().collect();
        let not_ours: HashSet<String> = ["c".to_string()].into_iter().collect();

        // Tracked ('b') and known-foreign ('c') are filtered; newest-first order kept.
        let picked = discovery_candidates(&recent, &tracked, &not_ours, 10);
        assert_eq!(picked, vec!["a", "d"]);

        // The cap bounds how many GETs one cycle can issue.
        let capped = discovery_candidates(&recent, &HashSet::new(), &HashSet::new(), 2);
        assert_eq!(capped, vec!["a", "b"]);
    }

    #[test]
    fn idle_backoff_is_bounded_near_the_floor() {
        // A quiet daemon must keep checking often enough that remote edits land
        // promptly. With the default 45s floor the worst case is 90s.
        let floor = Duration::from_secs(45);
        let ceil = idle_backoff_ceiling(floor);
        assert_eq!(ceil, Duration::from_secs(90));

        // Backoff doubles from the floor, lands on the ceiling, and never climbs past it.
        let mut delay = floor;
        delay = next_idle_delay(delay, ceil);
        assert_eq!(delay, Duration::from_secs(90));
        for _ in 0..10 {
            delay = next_idle_delay(delay, ceil);
            assert!(delay <= ceil, "idle backoff must never exceed the ceiling");
        }
        assert_eq!(delay, Duration::from_secs(90));
    }

    #[test]
    fn idle_ceiling_never_drops_below_a_long_floor() {
        // A deployment that intentionally polls slowly is still honored: the ceiling
        // can't fall below the configured floor, which would poll faster than asked.
        let floor = Duration::from_secs(120);
        assert_eq!(idle_backoff_ceiling(floor), floor);
        assert_eq!(next_idle_delay(floor, idle_backoff_ceiling(floor)), floor);
    }
}

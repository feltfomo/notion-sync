//! Startup reconciliation. Diff disk vs state.db vs Notion and converge.
//!
//! Order matters: directories first (so child pages have parents), then files.
//! Existing Notion pages at the expected path/title are ADOPTED (their id is stored
//! and the local-wins conflict policy decides content), never blindly recreated.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{info, warn};

use super::engine::Engine;
use super::util::{self, WalkEntry};
use crate::api::models::PageResp;
use crate::state::{Node, NodeKind};

pub async fn run(engine: Arc<Engine>) -> Result<(), String> {
    info!(
        mappings = engine.cfg.mappings.len(),
        "starting reconciliation pass"
    );

    // Walk every mapping up front, namespacing each entry by its mapping name so the
    // rest of reconcile (and state.db) sees one flat, collision-free path space.
    //
    // The empty/missing-tree mass-delete guard is applied PER MAPPING: pass 3 trashes
    // the Notion page for any tracked node absent from disk, so an unreadable root or
    // an empty walk (a mid-`mv` rename, an unmounted volume, a typo'd local_root) for
    // one mapping must not wipe that mapping's mirror -- and must not stall the others.
    // Only mappings that walked cleanly are "healthy" and eligible for deletions below.
    let mut on_disk: HashSet<String> = HashSet::new();
    let mut healthy: HashSet<String> = HashSet::new();
    let mut entries: Vec<WalkEntry> = Vec::new();
    for m in &engine.cfg.mappings {
        if !m.local_root.is_dir() {
            warn!(
                mapping = %m.name, root = %m.local_root.display(),
                "local_root is not a readable directory; skipping this mapping to avoid mass deletion"
            );
            continue;
        }
        let walked = match util::walk_async(m.local_root.clone(), m.ignore.clone()).await {
            Ok(w) => w,
            Err(e) => {
                warn!(mapping = %m.name, error = %e, "walk failed; skipping this mapping");
                continue;
            }
        };
        if walked.is_empty() {
            warn!(
                mapping = %m.name,
                "walk returned no entries; skipping this mapping to avoid trashing its mirror"
            );
            continue;
        }
        healthy.insert(m.name.clone());
        for mut e in walked {
            e.rel_path = format!("{}/{}", m.name, e.rel_path);
            entries.push(e);
        }
    }

    if healthy.is_empty() {
        warn!("no mapping produced a readable, non-empty tree; skipping reconcile to avoid mass deletion");
        return Ok(());
    }

    // Pass 1: directories (parents before children, guaranteed by walk order).
    for entry in entries.iter().filter(|e| e.is_dir) {
        on_disk.insert(entry.rel_path.clone());
        if let Err(e) = adopt_or_create_dir(&engine, entry).await {
            warn!(rel_path = %entry.rel_path, error = %e, "reconcile dir failed");
        }
    }

    // Pass 2: files.
    for entry in entries.iter().filter(|e| !e.is_dir) {
        on_disk.insert(entry.rel_path.clone());
        if let Err(e) = adopt_then_sync_file(&engine, entry).await {
            warn!(rel_path = %entry.rel_path, error = %e, "reconcile file failed");
        }
    }

    // Pass 3: tracked nodes whose local files are gone => delete from Notion.
    let tracked = {
        engine
            .state
            .lock()
            .await
            .all_tracked()
            .map_err(|e| e.to_string())?
    };
    // Only consider deletions for mappings that walked cleanly this pass. A node whose
    // mapping was skipped (missing/empty root) or is no longer configured at all is
    // left untouched rather than trashed, so a transient glitch or a removed mapping
    // never cascades into deleting its Notion pages.
    let missing: Vec<Node> = tracked
        .into_iter()
        .filter(|n| !on_disk.contains(&n.rel_path))
        .filter(|n| healthy.contains(n.rel_path.split('/').next().unwrap_or("")))
        .collect();

    // Mass-delete guard: a large-but-partial disappearance (an interrupted checkout, a
    // case-fold rename, a botched rebase) shouldn't silently trash a big slice of the
    // mirror. The whole-tree case is caught above; here, once missing files exceed ~20%
    // of a non-trivial tree, snapshot each page's current Notion body before trashing
    // it so the mass delete stays reversible, and log loudly.
    let total = on_disk.len() + missing.len();
    let ratio = if total == 0 {
        0.0
    } else {
        missing.len() as f64 / total as f64
    };
    let mass_delete = missing.len() >= 5 && ratio > 0.20;
    if mass_delete {
        warn!(
            missing = missing.len(),
            total,
            pct = (ratio * 100.0) as u32,
            "large fraction of the mirror is missing on disk; snapshotting each page before trashing"
        );
    }
    for node in &missing {
        if mass_delete {
            engine.snapshot_remote_before_delete(node).await;
        }
        info!(rel_path = %node.rel_path, "tracked node missing on disk; deleting");
        if let Err(e) = engine.handle_delete(&node.rel_path).await {
            warn!(rel_path = %node.rel_path, error = %e, "reconcile delete failed");
        }
    }
    info!("reconciliation complete");
    Ok(())
}

async fn adopt_or_create_dir(engine: &Arc<Engine>, entry: &WalkEntry) -> Result<(), String> {
    let tracked = {
        engine
            .state
            .lock()
            .await
            .get_by_path(&entry.rel_path)
            .ok()
            .flatten()
    };
    if tracked.is_some() {
        return Ok(());
    }
    // Try to adopt an existing child page with the matching title.
    if let Some(parent) = parent_page_for(engine, &entry.rel_path).await {
        let title = util::title_for(&entry.rel_path);
        if let Some(existing) = find_child_by_title(engine, &parent, title).await {
            let st = engine.state.lock().await;
            st.upsert(&Node {
                rel_path: entry.rel_path.clone(),
                kind: NodeKind::Dir,
                notion_page_id: existing.id,
                parent_page_id: parent,
                content_hash: None,
                body_block_ids: vec![],
                local_mtime_ns: None,
                notion_last_edited: Some(existing.last_edited_time),
                last_synced_dir: Some("adopted".into()),
                is_binary_placeholder: false,
            })
            .map_err(|e| e.to_string())?;
            info!(rel_path = %entry.rel_path, "adopted existing directory page");
            return Ok(());
        }
    }
    engine.ensure_dir(&entry.rel_path).await
}

async fn adopt_then_sync_file(engine: &Arc<Engine>, entry: &WalkEntry) -> Result<(), String> {
    let tracked = {
        engine
            .state
            .lock()
            .await
            .get_by_path(&entry.rel_path)
            .ok()
            .flatten()
    };
    if tracked.is_none() {
        // Adopt an existing page if one already sits at this path/title.
        if let Some(parent) = parent_page_for(engine, &entry.rel_path).await {
            let title = util::title_for(&entry.rel_path);
            if let Some(existing) = find_child_by_title(engine, &parent, title).await {
                let st = engine.state.lock().await;
                st.upsert(&Node {
                    rel_path: entry.rel_path.clone(),
                    kind: NodeKind::File,
                    notion_page_id: existing.id.clone(),
                    parent_page_id: parent,
                    // No hash yet => sync_file will overwrite from local (local-wins).
                    content_hash: None,
                    body_block_ids: existing.child_block_ids.clone(),
                    local_mtime_ns: None,
                    notion_last_edited: Some(existing.last_edited_time),
                    last_synced_dir: Some("adopted".into()),
                    is_binary_placeholder: false,
                })
                .map_err(|e| e.to_string())?;
                info!(rel_path = %entry.rel_path, "adopted existing file page");
            }
        }
    }
    // local-wins: push local content (creates if still untracked, overwrites if adopted).
    engine.sync_file(&entry.rel_path).await
}

async fn parent_page_for(engine: &Arc<Engine>, rel_path: &str) -> Option<String> {
    let parent_rel = util::parent_rel(rel_path);
    // A parent_rel that is exactly a mapping name means this node sits at that
    // mapping's root and hangs off its configured parent page.
    if let Some(m) = engine.cfg.mapping_by_name(&parent_rel) {
        return Some(m.parent_page_id.clone());
    }
    let st = engine.state.lock().await;
    st.get_by_path(&parent_rel)
        .ok()
        .flatten()
        .map(|n| n.notion_page_id)
}

async fn find_child_by_title(
    engine: &Arc<Engine>,
    parent_id: &str,
    title: &str,
) -> Option<AdoptInfo> {
    let blocks = engine.api.list_children(parent_id).await.ok()?;
    for b in blocks {
        if b.ty == "child_page" {
            if let Some(cp) = &b.child_page {
                if cp.title == title {
                    // Fetch page metadata + its body block ids for adoption.
                    let page: PageResp = engine.api.get_page(&b.id).await.ok()?;
                    let body = engine.api.list_children(&b.id).await.unwrap_or_default();
                    let child_block_ids = body
                        .iter()
                        .filter(|x| x.ty == "code")
                        .map(|x| x.id.clone())
                        .collect();
                    return Some(AdoptInfo {
                        id: page.id,
                        last_edited_time: page.last_edited_time,
                        child_block_ids,
                    });
                }
            }
        }
    }
    None
}

struct AdoptInfo {
    id: String,
    last_edited_time: String,
    child_block_ids: Vec<String>,
}

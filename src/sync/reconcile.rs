//! Startup reconciliation. Diff disk vs state.db vs Notion and converge.
//!
//! Order matters: directories first (so child pages have parents), then files.
//! Existing Notion pages at the expected path/title are ADOPTED (their id is stored
//! and the local-wins conflict policy decides content), never blindly recreated.

use std::collections::HashSet;
use std::sync::Arc;

use tracing::{info, warn};

use crate::api::models::PageResp;
use crate::state::{Node, NodeKind};
use super::engine::Engine;
use super::util::{self, WalkEntry};

pub async fn run(engine: Arc<Engine>) -> Result<(), String> {
    info!("starting reconciliation pass");
    let entries = util::walk(&engine.cfg.local_root, &engine.cfg.ignore).map_err(|e| e.to_string())?;

    // Safety guard: never treat a missing or empty tree as "the user deleted
    // everything." Pass 3 below trashes the Notion page for every tracked node that
    // is absent from disk, so an unreadable root or an empty walk (a mid-`mv` rename,
    // an unmounted volume, or a misconfigured local_root) would wipe the entire
    // mirror. Abort the pass instead and leave the mapping untouched.
    if !engine.cfg.local_root.is_dir() {
        return Err(format!(
            "local_root {} is not a readable directory; skipping reconcile to avoid mass deletion",
            engine.cfg.local_root.display()
        ));
    }
    if entries.is_empty() {
        warn!("local tree walk returned no entries; skipping reconcile to avoid trashing the mirror");
        return Ok(());
    }

    let mut on_disk: HashSet<String> = HashSet::new();

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
    let tracked = { engine.state.lock().await.all_tracked().map_err(|e| e.to_string())? };
    for node in tracked {
        if !on_disk.contains(&node.rel_path) {
            info!(rel_path = %node.rel_path, "tracked node missing on disk; deleting");
            if let Err(e) = engine.handle_delete(&node.rel_path).await {
                warn!(rel_path = %node.rel_path, error = %e, "reconcile delete failed");
            }
        }
    }
    info!("reconciliation complete");
    Ok(())
}

async fn adopt_or_create_dir(engine: &Arc<Engine>, entry: &WalkEntry) -> Result<(), String> {
    let tracked = { engine.state.lock().await.get_by_path(&entry.rel_path).ok().flatten() };
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
    let tracked = { engine.state.lock().await.get_by_path(&entry.rel_path).ok().flatten() };
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
    if parent_rel.is_empty() {
        return Some(engine.cfg.parent_page_id.clone());
    }
    let st = engine.state.lock().await;
    st.get_by_path(&parent_rel).ok().flatten().map(|n| n.notion_page_id)
}

async fn find_child_by_title(engine: &Arc<Engine>, parent_id: &str, title: &str) -> Option<AdoptInfo> {
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

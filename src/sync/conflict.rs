//! Local-wins conflict resolution for Notion -> local pulls. A Notion edit reaches
//! disk only if local hasn't diverged since our last push; on a real two-sided
//! conflict we keep local, stash the incoming copy under `.notion-sync/conflicts/`,
//! and re-push to restore the mirror.

use tracing::{info, warn};

use super::engine::{anyhow_lite, Engine};
use super::util;
use crate::hashutil;
use crate::state::Node;

pub async fn resolve_pull(engine: &Engine, node: &Node) -> anyhow_lite::Result {
    if node.is_binary_placeholder {
        return Ok(()); // placeholders are never written back
    }
    let abs = engine.abs_path(&node.rel_path);

    let local_bytes = tokio::fs::read(&abs).await.unwrap_or_default();
    let local_hash = hashutil::hash_bytes(&local_bytes);
    let local_unchanged = node.content_hash.as_deref() == Some(local_hash.as_str());

    let body = engine.read_page_body(node).await?;

    // A faithful mirror is pure code blocks. Foreign blocks mean a structured editor
    // split it (e.g. a .md file's own code fences parsed as real fences), so body.text
    // is a truncated reassembly. Never let that overwrite disk; repair from local.
    if body.foreign_blocks > 0 {
        if local_unchanged {
            warn!(rel_path = %node.rel_path, foreign = body.foreign_blocks,
                "notion page split into non-code blocks; re-pushing local to repair");
            return engine.force_push_locked(&node.rel_path).await;
        }
        warn!(rel_path = %node.rel_path, foreign = body.foreign_blocks,
            "notion page split AND local diverged; skipping pull, manual fix required");
        return Ok(());
    }

    let notion_content = body.text;
    let notion_hash = hashutil::hash_str(&notion_content);

    if notion_hash == local_hash {
        // Notion now matches disk: nothing to write, just refresh bookkeeping.
        return refresh_after_pull(engine, node, &local_hash).await;
    }

    if local_unchanged {
        // Snapshot the local copy before overwriting it from Notion (pre-pull backup),
        // so even a clean fast-forward stays reversible. Best-effort; never blocks.
        engine
            .capture(
                &node.rel_path,
                Some(&node.notion_page_id),
                "local",
                "pre-pull",
                local_bytes.clone(),
            )
            .await;
        // Clean fast-forward from Notion -> disk (async atomic write).
        util::atomic_write(&abs, notion_content.clone().into_bytes())
            .await
            .map_err(|e| e.to_string())?;
        info!(rel_path = %node.rel_path, "applied Notion edit to local file");
        engine
            .journal(
                &node.rel_path,
                "pull_overwrite",
                node.content_hash.as_deref(),
                Some(&notion_hash),
                "from_notion",
            )
            .await;
        return refresh_after_pull(engine, node, &notion_hash).await;
    }

    // True conflict: local changed AND Notion changed. Local wins.
    // Durable backup of the incoming Notion copy in the content-addressed store
    // (deduped, survives conflict-dir cleanup) plus a human-browsable file under
    // .notion-sync/conflicts/ for quick manual inspection.
    engine
        .capture(
            &node.rel_path,
            Some(&node.notion_page_id),
            "notion",
            "conflict",
            notion_content.clone().into_bytes(),
        )
        .await;
    let backup = backup_path(engine, &node.rel_path);
    if let Some(parent) = backup.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Err(e) = util::atomic_write(&backup, notion_content.clone().into_bytes()).await {
        warn!(rel_path = %node.rel_path, error = %e, "failed to write conflict backup");
    } else {
        warn!(rel_path = %node.rel_path, backup = %backup.display(),
            "conflict: local wins; backed up incoming Notion content");
    }
    engine
        .journal(
            &node.rel_path,
            "conflict",
            node.content_hash.as_deref(),
            Some(&notion_hash),
            "from_notion",
        )
        .await;
    // Re-push local to restore the mirror. Use force_push_locked, not sync_file: we
    // already hold this path's lock via pull_page, so sync_file would re-lock the same
    // mutex and deadlock; a full rebuild also clears any stray blocks.
    engine.force_push_locked(&node.rel_path).await
}

async fn refresh_after_pull(engine: &Engine, node: &Node, hash: &str) -> anyhow_lite::Result {
    let last = engine
        .api
        .get_page(&node.notion_page_id)
        .await
        .map_err(|e| e.to_string())?
        .last_edited_time;
    let mut updated = node.clone();
    updated.content_hash = Some(hash.to_string());
    updated.notion_last_edited = Some(last);
    updated.local_mtime_ns = util::file_mtime_ns(&engine.abs_path(&node.rel_path));
    updated.last_synced_dir = Some("from_notion".into());
    let st = engine.state.lock().await;
    st.upsert(&updated).map_err(|e| e.to_string())?;
    Ok(())
}

fn backup_path(engine: &Engine, rel_path: &str) -> std::path::PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    engine
        .cfg
        .local_root
        .join(".notion-sync")
        .join("conflicts")
        .join(format!("{rel_path}.{ts}"))
}

//! The sync engine: the single owner of state.db and the only place that mutates
//! Notion. The watcher and poller call into it; per-path locks serialize concurrent
//! work on the same node.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{info, warn};

use super::conflict;
use super::locks::PathLocks;
use super::util;
use crate::api::models::{BlockResp, CalloutBlockReq};
use crate::api::NotionClient;
use crate::chunk;
use crate::config::Config;
use crate::hashutil;
use crate::language;
use crate::state::{Node, NodeKind, State};

pub struct Engine {
    pub cfg: Config,
    pub api: Arc<NotionClient>,
    pub state: Arc<Mutex<State>>,
    pub locks: PathLocks,
    /// Our own integration's bot user id, for echo-loop suppression.
    pub bot_user_id: String,
}

/// A page body read back from Notion. `foreign_blocks` counts children that are
/// neither code blocks nor nested subpages; their presence means the single
/// code-block mirror was split apart (e.g. a structured edit of a file that carries
/// its own code fences), so `text` is an incomplete reassembly and must not be
/// trusted for a destructive disk write.
pub struct PageBody {
    pub text: String,
    pub foreign_blocks: usize,
}

impl Engine {
    pub fn abs_path(&self, rel: &str) -> PathBuf {
        self.cfg.local_root.join(rel)
    }

    /// Resolve the Notion parent page id for a node at `rel_path`.
    /// Root-level nodes hang off the configured parent page; nested nodes hang off
    /// the page of their parent directory (which must already be tracked).
    async fn parent_page_for(&self, rel_path: &str) -> Option<String> {
        let parent_rel = util::parent_rel(rel_path);
        if parent_rel.is_empty() {
            return Some(self.cfg.parent_page_id.clone());
        }
        let st = self.state.lock().await;
        st.get_by_path(&parent_rel)
            .ok()
            .flatten()
            .map(|n| n.notion_page_id)
    }

    /// Ensure a directory node exists as a subpage (idempotent).
    pub async fn ensure_dir(&self, rel_path: &str) -> anyhow_lite::Result {
        let lock = self.locks.lock_for(rel_path).await;
        let _g = lock.lock().await;

        {
            let st = self.state.lock().await;
            if st.get_by_path(rel_path).ok().flatten().is_some() {
                return Ok(());
            }
        }
        let parent = self
            .parent_page_for(rel_path)
            .await
            .ok_or_else(|| format!("no parent page for dir {rel_path}"))?;
        let title = util::title_for(rel_path).to_string();
        let page = self
            .api
            .create_page(&parent, &title, vec![])
            .await
            .map_err(|e| e.to_string())?;
        let st = self.state.lock().await;
        st.upsert(&Node {
            rel_path: rel_path.to_string(),
            kind: NodeKind::Dir,
            notion_page_id: page.id,
            parent_page_id: parent,
            content_hash: None,
            body_block_ids: vec![],
            local_mtime_ns: None,
            notion_last_edited: Some(page.last_edited_time),
            last_synced_dir: Some("to_notion".into()),
            is_binary_placeholder: false,
        })
        .map_err(|e| e.to_string())?;
        info!(rel_path, "created directory subpage");
        Ok(())
    }

    /// Sync a local file to Notion: create, update (overwrite), or detect a rename.
    pub async fn sync_file(&self, rel_path: &str) -> anyhow_lite::Result {
        let lock = self.locks.lock_for(rel_path).await;
        let _g = lock.lock().await;

        let abs = self.abs_path(rel_path);
        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(e) => {
                warn!(rel_path, error = %e, "file vanished before sync; treating as delete");
                return self.handle_delete_locked(rel_path).await;
            }
        };

        if bytes.len() as u64 > self.cfg.max_file_bytes {
            warn!(
                rel_path,
                size = bytes.len(),
                "file exceeds max_file_bytes; placeholder only"
            );
            return self.ensure_placeholder(rel_path, &bytes, "too large").await;
        }
        if util::looks_binary(&bytes) {
            return self.ensure_placeholder(rel_path, &bytes, "binary").await;
        }

        let content = String::from_utf8_lossy(&bytes).to_string();
        let hash = hashutil::hash_str(&content);
        let mtime_ns = file_mtime_ns(&abs);

        // Already up-to-date?
        let existing = { self.state.lock().await.get_by_path(rel_path).ok().flatten() };
        if let Some(node) = &existing {
            if node.content_hash.as_deref() == Some(hash.as_str()) {
                return Ok(()); // no change
            }
        }

        // Rename detection: a tracked file elsewhere with this exact hash and whose
        // local file is now gone => this is a rename/move, not a fresh create.
        if existing.is_none() {
            let candidate = { self.state.lock().await.get_by_hash(&hash).ok().flatten() };
            if let Some(old) = candidate {
                if old.rel_path != rel_path && !self.abs_path(&old.rel_path).exists() {
                    return self.handle_rename(&old, rel_path, &hash, mtime_ns).await;
                }
            }
        }

        match existing {
            Some(node) => {
                self.overwrite_body(&node, rel_path, &content, &hash, mtime_ns)
                    .await
            }
            None => {
                self.create_file_page(rel_path, &content, &hash, mtime_ns)
                    .await
            }
        }
    }

    async fn create_file_page(
        &self,
        rel_path: &str,
        content: &str,
        hash: &str,
        mtime_ns: Option<i64>,
    ) -> anyhow_lite::Result {
        let parent = self
            .parent_page_for(rel_path)
            .await
            .ok_or_else(|| format!("no parent page for {rel_path}"))?;
        let title = util::title_for(rel_path).to_string();
        let page = self
            .api
            .create_page(&parent, &title, vec![])
            .await
            .map_err(|e| e.to_string())?;
        let block_ids = self.append_body(&page.id, content, rel_path).await?;
        let last = self
            .api
            .get_page(&page.id)
            .await
            .map_err(|e| e.to_string())?
            .last_edited_time;
        let st = self.state.lock().await;
        st.upsert(&Node {
            rel_path: rel_path.to_string(),
            kind: NodeKind::File,
            notion_page_id: page.id,
            parent_page_id: parent,
            content_hash: Some(hash.to_string()),
            body_block_ids: block_ids,
            local_mtime_ns: mtime_ns,
            notion_last_edited: Some(last),
            last_synced_dir: Some("to_notion".into()),
            is_binary_placeholder: false,
        })
        .map_err(|e| e.to_string())?;
        info!(rel_path, "created file page");
        Ok(())
    }

    /// Overwrite strategy: delete old body blocks, append fresh, update state.
    ///
    /// Self-heals a stale mapping: if the tracked page was trashed out from under us,
    /// its blocks are archived and any append fails with "Can't edit block that is
    /// archived." In that case recreate the page from local instead of erroring.
    async fn overwrite_body(
        &self,
        node: &Node,
        rel_path: &str,
        content: &str,
        hash: &str,
        mtime_ns: Option<i64>,
    ) -> anyhow_lite::Result {
        // Verify the target page is still alive before mutating it.
        match self.api.get_page(&node.notion_page_id).await {
            Ok(page) if page.in_trash => {
                warn!(rel_path, page = %node.notion_page_id, "tracked page is in trash; recreating from local");
                return self
                    .create_file_page(rel_path, content, hash, mtime_ns)
                    .await;
            }
            Err(e) => {
                warn!(rel_path, page = %node.notion_page_id, error = %e, "tracked page unreadable (deleted?); recreating from local");
                return self
                    .create_file_page(rel_path, content, hash, mtime_ns)
                    .await;
            }
            Ok(_) => {}
        }
        for id in &node.body_block_ids {
            if let Err(e) = self.api.delete_block(id).await {
                warn!(rel_path, block = %id, error = %e, "failed to trash old block; continuing");
            }
        }
        let block_ids = self
            .append_body(&node.notion_page_id, content, rel_path)
            .await?;
        let last = self
            .api
            .get_page(&node.notion_page_id)
            .await
            .map_err(|e| e.to_string())?
            .last_edited_time;
        let mut updated = node.clone();
        updated.content_hash = Some(hash.to_string());
        updated.body_block_ids = block_ids;
        updated.local_mtime_ns = mtime_ns;
        updated.notion_last_edited = Some(last);
        updated.last_synced_dir = Some("to_notion".into());
        updated.is_binary_placeholder = false;
        let st = self.state.lock().await;
        st.upsert(&updated).map_err(|e| e.to_string())?;
        info!(rel_path, "updated file body (overwrite)");
        Ok(())
    }

    /// Append chunked code blocks; returns the ordered body block ids.
    async fn append_body(
        &self,
        page_id: &str,
        content: &str,
        rel_path: &str,
    ) -> anyhow_lite::Result<Vec<String>> {
        let lang = language::for_path(std::path::Path::new(rel_path));
        let blocks = chunk::encode(content, lang);
        let batches = chunk::batch_blocks(&blocks);
        let mut ids = Vec::new();
        for batch in batches {
            let mut got = self
                .api
                .append_children(page_id, batch)
                .await
                .map_err(|e| e.to_string())?;
            ids.append(&mut got);
        }
        Ok(ids)
    }

    /// Rename: keep the existing page, patch its title, reparent if the dir changed.
    async fn handle_rename(
        &self,
        old: &Node,
        new_rel: &str,
        hash: &str,
        mtime_ns: Option<i64>,
    ) -> anyhow_lite::Result {
        let new_title = util::title_for(new_rel).to_string();
        let new_parent = self
            .parent_page_for(new_rel)
            .await
            .ok_or_else(|| format!("no parent page for renamed {new_rel}"))?;
        let reparent = if new_parent != old.parent_page_id {
            Some(new_parent.as_str())
        } else {
            None
        };
        self.api
            .update_page(&old.notion_page_id, Some(&new_title), reparent, None)
            .await
            .map_err(|e| e.to_string())?;
        {
            let st = self.state.lock().await;
            st.rename_path(&old.rel_path, new_rel, &new_parent)
                .map_err(|e| e.to_string())?;
        }
        // Title/parent change does not alter the body; only update bookkeeping.
        let mut moved = old.clone();
        moved.rel_path = new_rel.to_string();
        moved.parent_page_id = new_parent;
        moved.content_hash = Some(hash.to_string());
        moved.local_mtime_ns = mtime_ns;
        moved.last_synced_dir = Some("to_notion".into());
        let st = self.state.lock().await;
        st.upsert(&moved).map_err(|e| e.to_string())?;
        info!(old = %old.rel_path, new = new_rel, "renamed page (preserved id + annotations)");
        Ok(())
    }

    /// Local delete => trash the page, drop the row.
    pub async fn handle_delete(&self, rel_path: &str) -> anyhow_lite::Result {
        let lock = self.locks.lock_for(rel_path).await;
        let _g = lock.lock().await;
        self.handle_delete_locked(rel_path).await
    }

    async fn handle_delete_locked(&self, rel_path: &str) -> anyhow_lite::Result {
        let node = { self.state.lock().await.get_by_path(rel_path).ok().flatten() };
        let Some(node) = node else { return Ok(()) };
        // A page is also a block; trashing the page trashes its subtree.
        if let Err(e) = self
            .api
            .update_page(&node.notion_page_id, None, None, Some(true))
            .await
        {
            warn!(rel_path, error = %e, "failed to trash page");
        }
        let st = self.state.lock().await;
        st.delete(rel_path).map_err(|e| e.to_string())?;
        info!(rel_path, "deleted (trashed page)");
        Ok(())
    }

    /// Create/refresh a binary (or oversized) placeholder page — no body, warning callout.
    async fn ensure_placeholder(
        &self,
        rel_path: &str,
        bytes: &[u8],
        reason: &str,
    ) -> anyhow_lite::Result {
        let existing = { self.state.lock().await.get_by_path(rel_path).ok().flatten() };
        if let Some(n) = &existing {
            if n.is_binary_placeholder {
                return Ok(()); // already a placeholder
            }
        }
        let parent = self
            .parent_page_for(rel_path)
            .await
            .ok_or_else(|| format!("no parent page for {rel_path}"))?;
        let title = util::title_for(rel_path).to_string();
        let msg = format!(
            "\u{26A0}\u{FE0F} {reason} file not synced ({} bytes). Source of truth remains local.",
            bytes.len()
        );
        let callout = serde_json::to_value(CalloutBlockReq::warning(msg)).unwrap();
        let page = self
            .api
            .create_page(&parent, &title, vec![callout])
            .await
            .map_err(|e| e.to_string())?;
        let st = self.state.lock().await;
        st.upsert(&Node {
            rel_path: rel_path.to_string(),
            kind: NodeKind::File,
            notion_page_id: page.id,
            parent_page_id: parent,
            content_hash: None,
            body_block_ids: vec![],
            local_mtime_ns: file_mtime_ns(&self.abs_path(rel_path)),
            notion_last_edited: Some(page.last_edited_time),
            last_synced_dir: Some("to_notion".into()),
            is_binary_placeholder: true,
        })
        .map_err(|e| e.to_string())?;
        info!(
            rel_path,
            reason, "created binary/oversized placeholder page"
        );
        Ok(())
    }

    /// Read + reassemble a page's body into file bytes (Notion -> local).
    ///
    /// Only code blocks form the body, in order. Any other block type (paragraph,
    /// heading, ...) means an editor split the mirror code block, which happens when
    /// a file carries its own code fences; we count those as `foreign_blocks` so the
    /// caller can refuse the pull instead of writing a truncated reassembly.
    pub async fn read_page_body(&self, node: &Node) -> anyhow_lite::Result<PageBody> {
        let blocks: Vec<BlockResp> = self
            .api
            .list_children(&node.notion_page_id)
            .await
            .map_err(|e| e.to_string())?;
        let mut per_block: Vec<Vec<String>> = Vec::new();
        let mut foreign_blocks = 0usize;
        for b in &blocks {
            match b.ty.as_str() {
                "code" => per_block.push(
                    b.code
                        .as_ref()
                        .map(|c| c.rich_text.iter().map(|r| r.plain_text.clone()).collect())
                        .unwrap_or_default(),
                ),
                "child_page" => {} // nested file/dir subpage, not part of this body
                _ => foreign_blocks += 1,
            }
        }
        Ok(PageBody {
            text: chunk::reassemble(&per_block),
            foreign_blocks,
        })
    }

    /// Rebuild a page's entire body from the local file, deleting *every* existing
    /// child block first (not just tracked ones) so blocks introduced by a structured
    /// edit don't survive the repair. `overwrite_body` only touches tracked blocks and
    /// would leave foreign ones behind. Caller must already hold the per-path lock;
    /// resolve_pull runs under the lock taken by `pull_page`.
    pub async fn force_push_locked(&self, rel_path: &str) -> anyhow_lite::Result {
        let node = { self.state.lock().await.get_by_path(rel_path).ok().flatten() };
        let Some(node) = node else { return Ok(()) };
        let abs = self.abs_path(rel_path);
        let bytes = std::fs::read(&abs).map_err(|e| e.to_string())?;
        let content = String::from_utf8_lossy(&bytes).to_string();
        let hash = hashutil::hash_str(&content);
        let mtime_ns = file_mtime_ns(&abs);

        let children = self
            .api
            .list_children(&node.notion_page_id)
            .await
            .map_err(|e| e.to_string())?;
        for b in children {
            if let Err(e) = self.api.delete_block(&b.id).await {
                warn!(rel_path, block = %b.id, error = %e, "failed to trash block during repair; continuing");
            }
        }
        let block_ids = self
            .append_body(&node.notion_page_id, &content, rel_path)
            .await?;
        let last = self
            .api
            .get_page(&node.notion_page_id)
            .await
            .map_err(|e| e.to_string())?
            .last_edited_time;
        let mut updated = node.clone();
        updated.content_hash = Some(hash);
        updated.body_block_ids = block_ids;
        updated.local_mtime_ns = mtime_ns;
        updated.notion_last_edited = Some(last);
        updated.last_synced_dir = Some("to_notion".into());
        updated.is_binary_placeholder = false;
        let st = self.state.lock().await;
        st.upsert(&updated).map_err(|e| e.to_string())?;
        warn!(
            rel_path,
            "repaired split page: rebuilt body as a clean code-block mirror"
        );
        Ok(())
    }

    /// Pull a Notion edit down to disk, applying the local-wins conflict policy.
    pub async fn pull_page(&self, node: &Node) -> anyhow_lite::Result {
        let lock = self.locks.lock_for(&node.rel_path).await;
        let _g = lock.lock().await;
        conflict::resolve_pull(self, node).await
    }
}

fn file_mtime_ns(path: &std::path::Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_nanos() as i64)
}

/// A tiny local Result alias + error so we don't pull in the `anyhow` crate
/// (keeps the dependency tree lean per the hard constraints).
pub mod anyhow_lite {
    pub type Result<T = ()> = std::result::Result<T, String>;
}

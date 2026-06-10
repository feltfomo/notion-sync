//! The sync engine: the single owner of state.db and the only place that mutates
//! Notion. The watcher and poller call into it; per-path locks serialize concurrent
//! work on the same node.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::conflict;
use super::locks::PathLocks;
use super::snapshot::ObjectStore;
use super::util;
use crate::api::models::{BlockResp, CalloutBlockReq, PageResp};
use crate::api::NotionClient;
use crate::chunk;
use crate::config::Config;
use crate::hashutil;
use crate::language;
use crate::state::{Node, NodeKind, State};

/// How long a self-write record stays live. Generous relative to the watcher
/// debounce/tick so the echoed filesystem event is always still suppressible, yet
/// short enough that a genuine user edit landing seconds later is not swallowed.
const SELF_WRITE_TTL: Duration = Duration::from_secs(10);

pub struct Engine {
    pub cfg: Config,
    pub api: Arc<NotionClient>,
    pub state: Arc<Mutex<State>>,
    pub locks: PathLocks,
    /// Content-addressed snapshot store backing backup/restore + the sync journal.
    pub store: ObjectStore,
    /// Our own integration's bot user id, for echo-loop suppression.
    pub bot_user_id: String,
    /// Paths the daemon itself just wrote to disk during a pull, mapped to
    /// (hash_we_wrote, when), so the watcher can skip its own pull-writes instead of
    /// echoing them back to Notion.
    pub self_writes: Mutex<HashMap<String, (String, Instant)>>,
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

/// Reassemble a page's code-block runs into file bytes in document order, counting
/// foreign (non-code, non-subpage) blocks so the caller can refuse a truncated pull.
/// Split out of read_page_body so it's testable without the network.
fn reassemble_page_body(blocks: Vec<BlockResp>) -> PageBody {
    let mut text = String::new();
    let mut foreign_blocks = 0usize;
    for b in blocks {
        let BlockResp { ty, code, .. } = b;
        match ty.as_str() {
            "code" => {
                if let Some(code) = code {
                    for run in code.rich_text {
                        text.push_str(&run.plain_text);
                    }
                }
            }
            "child_page" => {} // nested file/dir subpage, not part of this body
            _ => foreign_blocks += 1,
        }
    }
    PageBody {
        text,
        foreign_blocks,
    }
}

/// Outcome of a discovery probe, so the poller can tell "foreign page, stop asking"
/// from "ours but not mirrorable yet, ask again". The webhook path ignores it.
pub enum Discovery {
    /// A new local file was written and tracked.
    Created,
    /// The id was already tracked; nothing to do.
    AlreadyTracked,
    /// Not part of any mapping tree (or an unusable title). Safe to stop re-probing.
    NotPlaceable,
    /// Placeable but skipped for now (empty, or split into non-code blocks). May gain a
    /// real body later, so callers should NOT cache it as foreign.
    Skipped,
}

/// Sanitize a Notion page title into a single safe path segment, or None if it can't be
/// one. A discovered title becomes a filename, so anything that could escape the mapping
/// root or name an entry we don't mean (path separators, the `.`/`..` entries, an
/// embedded NUL, an empty/whitespace title) is rejected rather than coerced.
fn sanitize_segment(title: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() || t == "." || t == ".." {
        return None;
    }
    if t.contains('/') || t.contains('\\') || t.contains('\0') {
        return None;
    }
    Some(t.to_string())
}

/// Notion ids come in two forms: the dashed UUID the API returns
/// (`378f23c5-af95-...`) and the compact 32-char hex a user typically pastes into a
/// config `parent_page_id` (`378f23c5af95...`). Compare them dash- and case-insensitively
/// so a mapping root written either way still matches the dashed parent id the API hands
/// back. Without this, discovery never recognizes a mapping root and silently treats every
/// new page as un-placeable.
fn normalize_page_id(id: &str) -> String {
    id.replace('-', "").to_lowercase()
}

impl Engine {
    pub fn abs_path(&self, rel: &str) -> PathBuf {
        // A namespaced rel_path is `<mapping name>/<path within that mapping's root>`.
        // Resolve the mapping by its leading segment and join the remainder onto the
        // mapping's local_root. A bare segment (no '/') is the mapping root itself.
        match rel.split_once('/') {
            Some((name, within)) => match self.cfg.mapping_by_name(name) {
                Some(m) => m.local_root.join(within),
                None => PathBuf::from(rel),
            },
            None => match self.cfg.mapping_by_name(rel) {
                Some(m) => m.local_root.clone(),
                None => PathBuf::from(rel),
            },
        }
    }

    /// The size cap for the file at `rel`, taken from its owning mapping (a per-dir
    /// `.notion-sync.toml` can override it), falling back to the central default for a
    /// path that resolves to no mapping.
    fn max_file_bytes_for(&self, rel: &str) -> u64 {
        self.cfg
            .mapping_for_path(rel)
            .map(|m| m.max_file_bytes)
            .unwrap_or(self.cfg.max_file_bytes)
    }

    /// Record that the daemon just wrote `hash` to `rel_path` on disk (a pull
    /// fast-forward), so the watcher can recognize and skip the filesystem event the
    /// write triggers rather than echoing it straight back to Notion.
    pub(crate) async fn note_self_write(&self, rel_path: &str, hash: &str) {
        let mut map = self.self_writes.lock().await;
        map.insert(rel_path.to_string(), (hash.to_string(), Instant::now()));
    }

    /// True if the daemon recently wrote exactly `current_hash` to `rel_path` itself.
    /// Consumes the matching record and evicts expired ones so the map stays bounded.
    pub(crate) async fn is_self_write(&self, rel_path: &str, current_hash: &str) -> bool {
        let mut map = self.self_writes.lock().await;
        map.retain(|_, (_, at)| at.elapsed() < SELF_WRITE_TTL);
        match map.get(rel_path) {
            Some((hash, _)) if hash == current_hash => {
                map.remove(rel_path);
                true
            }
            _ => false,
        }
    }

    /// Snapshot `bytes` for `rel_path` into the content-addressed object store and
    /// record an index row. `side` is which copy was saved ("local" or "notion");
    /// `reason` is the trigger ("pre-push"/"pre-pull"/"conflict"/"manual"/"pre-restore").
    /// Returns the blake3 hash of the saved blob. Best-effort by design: a snapshot
    /// failure must never block the sync it protects, so errors are logged, not bubbled.
    pub(crate) async fn capture(
        &self,
        rel_path: &str,
        page_id: Option<&str>,
        side: &str,
        reason: &str,
        bytes: Vec<u8>,
    ) -> Option<String> {
        let size = bytes.len() as i64;
        let hash = match self.store.put(bytes).await {
            Ok(h) => h,
            Err(e) => {
                warn!(rel_path, side, reason, error = %e, "snapshot blob write failed; continuing");
                return None;
            }
        };
        {
            let st = self.state.lock().await;
            if let Err(e) = st.insert_snapshot(rel_path, page_id, side, &hash, size, reason) {
                warn!(rel_path, error = %e, "snapshot index insert failed; continuing");
            }
        }
        Some(hash)
    }

    /// Append an audit row to the sync journal (best-effort; never blocks a sync).
    pub(crate) async fn journal(
        &self,
        rel_path: &str,
        action: &str,
        from_hash: Option<&str>,
        to_hash: Option<&str>,
        side: &str,
    ) {
        let st = self.state.lock().await;
        if let Err(e) = st.insert_journal(rel_path, action, from_hash, to_hash, side) {
            warn!(rel_path, action, error = %e, "journal insert failed; continuing");
        }
    }

    /// Resolve the Notion parent page id for a node at `rel_path`.
    /// Root-level nodes hang off the configured parent page; nested nodes hang off
    /// the page of their parent directory (which must already be tracked).
    pub(crate) async fn parent_page_for(&self, rel_path: &str) -> Option<String> {
        let parent_rel = util::parent_rel(rel_path);
        // A node whose parent_rel is exactly a mapping name sits at that mapping's root,
        // so it hangs off the mapping's configured parent page. Otherwise the parent is
        // another tracked node (a directory subpage).
        if let Some(m) = self.cfg.mapping_by_name(&parent_rel) {
            return Some(m.parent_page_id.clone());
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
        let bytes = match tokio::fs::read(&abs).await {
            Ok(b) => b,
            Err(e) => {
                warn!(rel_path, error = %e, "file vanished before sync; treating as delete");
                return self.handle_delete_locked(rel_path).await;
            }
        };

        if bytes.len() as u64 > self.max_file_bytes_for(rel_path) {
            warn!(
                rel_path,
                size = bytes.len(),
                "file exceeds max_file_bytes; placeholder only"
            );
            return self.ensure_placeholder(rel_path, &bytes, "too large").await;
        }
        // Sniff and decode in one pass: a NUL in the first 8 KiB means binary (checked
        // before any UTF-8 scan, preserving the old precedence), otherwise the UTF-8
        // decode that proves it is text also hands back the String we mirror -- so we
        // validate UTF-8 once here instead of in looks_binary and again in from_utf8.
        // The binary arm gets the bytes back for the placeholder.
        let content = match util::classify_text(bytes) {
            util::TextOrBinary::Text(text) => text,
            util::TextOrBinary::Binary(bytes) => {
                return self.ensure_placeholder(rel_path, &bytes, "binary").await;
            }
        };
        let hash = hashutil::hash_str(&content);
        let mtime_ns = util::file_mtime_ns(&abs);

        // Echo guard: if this exact content is what the daemon just wrote during a pull,
        // the filesystem event is our own write. Skip it instead of pushing it back.
        if self.is_self_write(rel_path, &hash).await {
            return Ok(());
        }

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
        drop(st);
        self.journal(rel_path, "create", None, Some(hash), "to_notion")
            .await;
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
            Ok(page) if page.trashed() => {
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
        // Pre-push backup: save the current Notion body before overwriting it, so a bad
        // local edit that clobbers good remote content stays recoverable. Best-effort:
        // a capture failure never blocks the push.
        let prev_hash = match self.read_page_body(node).await {
            Ok(body) => {
                self.capture(
                    rel_path,
                    Some(&node.notion_page_id),
                    "notion",
                    "pre-push",
                    body.text.into_bytes(),
                )
                .await
            }
            Err(e) => {
                warn!(rel_path, error = %e, "could not read remote body for pre-push snapshot; continuing");
                None
            }
        };
        // Append the fresh body BEFORE trashing the old blocks. If we deleted first and
        // the append failed, the page would be left blank with the content gone.
        // Appending first leaves the old body intact on failure; the worst case is a
        // transient duplicate the next sync cleans up.
        //
        // Only the tracked body blocks are trashed, not arbitrary children: human-added
        // foreign blocks survive a plain push and are cleaned by the force_push rebuild
        // in resolve_pull on the next pull.
        let block_ids = self
            .append_body(&node.notion_page_id, content, rel_path)
            .await?;
        for id in &node.body_block_ids {
            if let Err(e) = self.api.delete_block(id).await {
                warn!(rel_path, block = %id, error = %e, "failed to trash old block; continuing");
            }
        }
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
        drop(st);
        self.journal(
            rel_path,
            "push_overwrite",
            prev_hash.as_deref(),
            Some(hash),
            "to_notion",
        )
        .await;
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
        let batches = chunk::batch_blocks(blocks);
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
        // Trashing a directory page also trashes every descendant page in Notion, so
        // the matching state rows must go too, or they linger as orphans pointing at
        // trashed pages. For a plain file, delete_subtree matches only its own row.
        let removed = {
            let st = self.state.lock().await;
            st.delete_subtree(rel_path).map_err(|e| e.to_string())?
        };
        for n in &removed {
            self.journal(&n.rel_path, "delete", None, None, "to_notion")
                .await;
        }
        info!(
            rel_path,
            removed = removed.len(),
            "deleted (trashed page + descendant rows)"
        );
        Ok(())
    }

    /// Apply a remote deletion (the Notion page was trashed) to the local mirror:
    /// snapshot the local file for recovery, remove it from disk, and drop its rows.
    /// The snapshot is best-effort; the unlink + row removal are the durable effects.
    pub async fn handle_remote_delete(&self, node: &Node) -> anyhow_lite::Result {
        let lock = self.locks.lock_for(&node.rel_path).await;
        let _g = lock.lock().await;
        let abs = self.abs_path(&node.rel_path);
        if let Ok(bytes) = tokio::fs::read(&abs).await {
            self.capture(
                &node.rel_path,
                Some(&node.notion_page_id),
                "local",
                "remote-delete",
                bytes,
            )
            .await;
        }
        if let Err(e) = tokio::fs::remove_file(&abs).await {
            warn!(rel_path = %node.rel_path, error = %e, "failed to remove local file for remote delete; continuing");
        }
        let removed = {
            let st = self.state.lock().await;
            st.delete_subtree(&node.rel_path)
                .map_err(|e| e.to_string())?
        };
        for n in &removed {
            self.journal(&n.rel_path, "remote_delete", None, None, "from_notion")
                .await;
        }
        info!(rel_path = %node.rel_path, removed = removed.len(), "applied remote delete to local mirror");
        Ok(())
    }

    /// Snapshot a page's current Notion body before it is trashed, so a deletion stays
    /// recoverable. Used by the reconcile mass-delete guard. Best-effort.
    pub async fn snapshot_remote_before_delete(&self, node: &Node) {
        if node.kind != NodeKind::File || node.is_binary_placeholder {
            return;
        }
        match self.read_page_body(node).await {
            Ok(body) => {
                self.capture(
                    &node.rel_path,
                    Some(&node.notion_page_id),
                    "notion",
                    "pre-delete",
                    body.text.into_bytes(),
                )
                .await;
            }
            Err(e) => {
                warn!(rel_path = %node.rel_path, error = %e, "pre-delete snapshot read failed; continuing")
            }
        }
    }

    /// Create/refresh a binary (or oversized) placeholder page: no body, warning callout.
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
            "{reason} file not synced ({} bytes). Source of truth remains local.",
            bytes.len()
        );
        let callout = serde_json::to_value(CalloutBlockReq::warning(msg)).unwrap();

        // If a real page already exists at this path (a file that was text and is now
        // binary/oversized, or an adopted page), REUSE it. A fresh page would orphan the
        // original, leaving an untrashed duplicate racing for the path.
        if let Some(node) = existing {
            match self.api.get_page(&node.notion_page_id).await {
                Ok(page) if !page.trashed() => {
                    // Append the placeholder callout first, then clear the old body
                    // (same append-before-delete safety as overwrite_body).
                    let new_ids = self
                        .api
                        .append_children(&node.notion_page_id, vec![callout])
                        .await
                        .map_err(|e| e.to_string())?;
                    for id in &node.body_block_ids {
                        if let Err(e) = self.api.delete_block(id).await {
                            warn!(rel_path, block = %id, error = %e, "failed to trash old block; continuing");
                        }
                    }
                    let last = self
                        .api
                        .get_page(&node.notion_page_id)
                        .await
                        .map_err(|e| e.to_string())?
                        .last_edited_time;
                    let mut updated = node.clone();
                    updated.content_hash = None;
                    updated.body_block_ids = new_ids;
                    updated.local_mtime_ns = util::file_mtime_ns(&self.abs_path(rel_path));
                    updated.notion_last_edited = Some(last);
                    updated.last_synced_dir = Some("to_notion".into());
                    updated.is_binary_placeholder = true;
                    let st = self.state.lock().await;
                    st.upsert(&updated).map_err(|e| e.to_string())?;
                    info!(
                        rel_path,
                        reason, "converted existing page to binary/oversized placeholder"
                    );
                    return Ok(());
                }
                _ => {} // page gone/trashed: fall through and create a fresh one
            }
        }

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
            local_mtime_ns: util::file_mtime_ns(&self.abs_path(rel_path)),
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
        self.read_page_body_by_id(&node.notion_page_id).await
    }

    /// Read + reassemble a page's body by its Notion page id, without a tracked Node.
    /// The webhook discovery path has only a bare page id from the event payload, so it
    /// reuses this directly; `read_page_body` is the Node-keyed convenience wrapper.
    pub async fn read_page_body_by_id(&self, page_id: &str) -> anyhow_lite::Result<PageBody> {
        let blocks: Vec<BlockResp> = self
            .api
            .list_children(page_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(reassemble_page_body(blocks))
    }

    /// True if `node`'s current remote body still hashes to exactly what we last synced.
    /// This is the content half of echo suppression: an edit attributed to our bot is a
    /// real echo of our own write only if the body still matches. A page with foreign
    /// blocks (a split mirror) or one we can't read is never an echo, so it falls through
    /// to be pulled or repaired. The caller owns the bot-attribution check, since the
    /// poller reads it from `last_edited_by` and the webhook worker from event `authors`.
    pub async fn remote_body_matches_last_sync(&self, node: &Node) -> bool {
        match self.read_page_body(node).await {
            Ok(body) if body.foreign_blocks == 0 => {
                node.content_hash.as_deref() == Some(hashutil::hash_str(&body.text).as_str())
            }
            _ => false,
        }
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
        let bytes = tokio::fs::read(&abs).await.map_err(|e| e.to_string())?;
        // Mirror sync_file's guards: never push an oversized or binary file as corrupted
        // text; emit a placeholder instead. classify_text sniffs and decodes in one pass,
        // surfacing invalid UTF-8 as binary rather than mangling it.
        if bytes.len() as u64 > self.max_file_bytes_for(rel_path) {
            return self.ensure_placeholder(rel_path, &bytes, "too large").await;
        }
        let content = match util::classify_text(bytes) {
            util::TextOrBinary::Text(text) => text,
            util::TextOrBinary::Binary(bytes) => {
                return self.ensure_placeholder(rel_path, &bytes, "binary").await;
            }
        };
        let hash = hashutil::hash_str(&content);
        let mtime_ns = util::file_mtime_ns(&abs);

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

    /// Where an untracked page belongs on disk, if anywhere. The parent must resolve to
    /// a known anchor: a mapping's configured root page (=> `<mapping>/<title>`) or an
    /// already-tracked directory node (=> `<dir>/<title>`). A parent that is itself an
    /// untracked page is deliberately NOT followed -- placing a file under a dir we
    /// don't track yet would leave state.db inconsistent; reconcile owns that case.
    async fn placement_for(&self, page: &PageResp) -> Option<String> {
        let parent_id = page.parent_page_id()?;
        let name = sanitize_segment(&page.title())?;
        // Config ids are commonly compact; API parent ids are dashed. Normalize both.
        let parent_norm = normalize_page_id(parent_id);
        if let Some(m) = self
            .cfg
            .mappings
            .iter()
            .find(|m| normalize_page_id(&m.parent_page_id) == parent_norm)
        {
            return Some(format!("{}/{}", m.name, name));
        }
        let parent = {
            self.state
                .lock()
                .await
                .get_by_page_id(parent_id)
                .ok()
                .flatten()
        };
        match parent {
            Some(n) if n.kind == NodeKind::Dir => Some(format!("{}/{}", n.rel_path, name)),
            _ => None,
        }
    }

    /// Mirror an untracked Notion page to disk from just its page id (all the webhook or
    /// poller has). Conservative: only a page that places under a tracked parent chain
    /// AND reads back as a faithful, non-empty code body becomes a file. Trashed pages,
    /// foreign-block pages, empty pages, and pages outside any mapping are left alone.
    /// Idempotent under the per-path lock, so a created+content_updated burst (or a
    /// webhook racing the poller) can't double-create.
    ///
    /// Echo note: a page WE just pushed is tracked by the push itself, so the early
    /// tracked-check no-ops it. Callers that have author info (webhook event authors,
    /// poller last_edited_by) still pre-filter so we don't even probe our own writes.
    pub async fn discover_remote_page(&self, page_id: &str) -> anyhow_lite::Result<Discovery> {
        if self
            .state
            .lock()
            .await
            .get_by_page_id(page_id)
            .ok()
            .flatten()
            .is_some()
        {
            return Ok(Discovery::AlreadyTracked);
        }
        let page = self
            .api
            .get_page(page_id)
            .await
            .map_err(|e| e.to_string())?;
        if page.trashed() {
            return Ok(Discovery::NotPlaceable);
        }
        let Some(rel_path) = self.placement_for(&page).await else {
            return Ok(Discovery::NotPlaceable);
        };

        let lock = self.locks.lock_for(&rel_path).await;
        let _g = lock.lock().await;
        // Re-check under the lock: another discovery (or the push that created this page)
        // may have tracked the path or id while we were resolving placement.
        {
            let st = self.state.lock().await;
            if st.get_by_path(&rel_path).ok().flatten().is_some()
                || st.get_by_page_id(page_id).ok().flatten().is_some()
            {
                return Ok(Discovery::AlreadyTracked);
            }
        }

        // Read the body once: collect the ordered code-block ids (for later overwrites)
        // and reassemble the text in the same pass.
        let blocks = self
            .api
            .list_children(page_id)
            .await
            .map_err(|e| e.to_string())?;
        let block_ids: Vec<String> = blocks
            .iter()
            .filter(|b| b.ty == "code")
            .map(|b| b.id.clone())
            .collect();
        let body = reassemble_page_body(blocks);
        if body.foreign_blocks > 0 {
            warn!(rel_path, page = %page_id, foreign = body.foreign_blocks,
                "discovered page split into non-code blocks; not mirroring (leaving to reconcile/manual)");
            return Ok(Discovery::Skipped);
        }
        if body.text.is_empty() {
            // An empty page is indistinguishable from an empty file here, and we chose not
            // to materialize empty files from remote. Skip; it may gain a body later.
            debug!(rel_path, page = %page_id, "discovered page has no code body yet; skipping");
            return Ok(Discovery::Skipped);
        }

        let content = body.text;
        let hash = hashutil::hash_str(&content);
        let abs = self.abs_path(&rel_path);
        // Mark our own write before touching disk so the watcher reads the resulting fs
        // event as an echo instead of a fresh local change to push straight back.
        self.note_self_write(&rel_path, &hash).await;
        util::atomic_write(&abs, content.into_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let parent_page_id = page.parent_page_id().unwrap_or_default().to_string();
        let st = self.state.lock().await;
        st.upsert(&Node {
            rel_path: rel_path.clone(),
            kind: NodeKind::File,
            notion_page_id: page.id.clone(),
            parent_page_id,
            content_hash: Some(hash.clone()),
            body_block_ids: block_ids,
            local_mtime_ns: util::file_mtime_ns(&abs),
            notion_last_edited: Some(page.last_edited_time.clone()),
            last_synced_dir: Some("from_notion".into()),
            is_binary_placeholder: false,
        })
        .map_err(|e| e.to_string())?;
        drop(st);
        self.journal(&rel_path, "discover", None, Some(&hash), "from_notion")
            .await;
        info!(rel_path, page = %page_id, "discovered untracked Notion page; created local file");
        Ok(Discovery::Created)
    }
}

/// A tiny local Result alias + error so we don't pull in the `anyhow` crate
/// (keeps the dependency tree lean per the hard constraints).
pub mod anyhow_lite {
    pub type Result<T = ()> = std::result::Result<T, String>;
}

#[cfg(test)]
mod tests {
    use super::normalize_page_id;
    use super::reassemble_page_body;
    use super::sanitize_segment;
    use crate::api::models::BlockResp;

    #[test]
    fn normalize_page_id_is_dash_and_case_insensitive() {
        // The config commonly holds a compact 32-char id; the API returns a dashed,
        // sometimes upper-cased UUID. Discovery must see them as the same root.
        let compact = "378f23c5af9580a59a6dc218fa24b366";
        let dashed = "378F23C5-AF95-80A5-9A6D-C218FA24B366";
        assert_eq!(normalize_page_id(dashed), compact);
        assert_eq!(normalize_page_id(compact), compact);
        assert_eq!(normalize_page_id(dashed), normalize_page_id(compact));
    }

    #[test]
    fn sanitize_segment_trims_and_rejects_traversal_and_separators() {
        assert_eq!(sanitize_segment("main.rs").as_deref(), Some("main.rs"));
        assert_eq!(sanitize_segment("  notes  ").as_deref(), Some("notes"));
        // A title that's empty, whitespace, or the dot entries can't name a file.
        assert!(sanitize_segment("").is_none());
        assert!(sanitize_segment("   ").is_none());
        assert!(sanitize_segment(".").is_none());
        assert!(sanitize_segment("..").is_none());
        // Separators would escape the mapping root / name a nested entry we don't mean.
        assert!(sanitize_segment("a/b").is_none());
        assert!(sanitize_segment("a\\b").is_none());
    }

    fn blocks_from(json: serde_json::Value) -> Vec<BlockResp> {
        serde_json::from_value(json).expect("valid block fixtures")
    }

    #[test]
    fn reassembles_code_in_order_skips_child_pages_counts_foreign() {
        // Code runs concatenate in document order. child_page subpages are skipped
        // without counting; any other block type is foreign, which lets the caller
        // refuse to overwrite disk with a truncated reassembly.
        let blocks = blocks_from(serde_json::json!([
            {"id": "b1", "type": "code", "last_edited_time": "t",
             "code": {"rich_text": [{"plain_text": "fn main() {\n"}, {"plain_text": "\tok();\n"}], "language": "rust"}},
            {"id": "b2", "type": "child_page", "last_edited_time": "t", "child_page": {"title": "nested"}},
            {"id": "b3", "type": "paragraph", "last_edited_time": "t"},
            {"id": "b4", "type": "code", "last_edited_time": "t",
             "code": {"rich_text": [{"plain_text": "}\n"}], "language": "rust"}}
        ]));
        let body = reassemble_page_body(blocks);
        assert_eq!(body.text, "fn main() {\n\tok();\n}\n");
        assert_eq!(body.foreign_blocks, 1);
    }

    #[test]
    fn empty_page_reassembles_to_empty_with_no_foreign() {
        let body = reassemble_page_body(blocks_from(serde_json::json!([])));
        assert_eq!(body.text, "");
        assert_eq!(body.foreign_blocks, 0);
    }
}

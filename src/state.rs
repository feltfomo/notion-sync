//! SQLite-backed sync state (`state.db`).
//!
//! This DB is machine-local, on purpose. There's no shared or remote state, so
//! pointing two daemons at the same Notion tree from different machines will corrupt
//! the mapping in v1. Don't design around it.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    File,
    Dir,
}

impl NodeKind {
    fn as_str(&self) -> &'static str {
        match self {
            NodeKind::File => "file",
            NodeKind::Dir => "dir",
        }
    }
    fn parse(s: &str) -> NodeKind {
        if s == "dir" {
            NodeKind::Dir
        } else {
            NodeKind::File
        }
    }
}

#[derive(Debug, Clone)]
pub struct Node {
    pub rel_path: String,
    pub kind: NodeKind,
    pub notion_page_id: String,
    pub parent_page_id: String,
    pub content_hash: Option<String>,
    /// JSON array of body code-block ids, in order.
    pub body_block_ids: Vec<String>,
    pub local_mtime_ns: Option<i64>,
    pub notion_last_edited: Option<String>,
    pub last_synced_dir: Option<String>,
    /// True for binary files mirrored only as a placeholder page.
    pub is_binary_placeholder: bool,
}

pub struct State {
    conn: Connection,
}

impl State {
    /// Open (creating if needed) the state DB under $XDG_STATE_HOME/notion-sync/.
    pub fn open_default() -> rusqlite::Result<State> {
        let path = default_db_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        State::open(&path)
    }

    pub fn open(path: &Path) -> rusqlite::Result<State> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let s = State { conn };
        s.migrate()?;
        Ok(s)
    }

    /// In-memory DB for tests.
    pub fn open_in_memory() -> rusqlite::Result<State> {
        let conn = Connection::open_in_memory()?;
        let s = State { conn };
        s.migrate()?;
        Ok(s)
    }

    /// Versioned migration runner. Each step is applied in order based on
    /// `PRAGMA user_version`, so future column/table additions stay possible on an
    /// already-populated DB instead of relying on bare CREATE TABLE IF NOT EXISTS
    /// (which can never ALTER). An existing v0.1 DB has user_version 0 and a `nodes`
    /// table; step 1's CREATE ... IF NOT EXISTS is then a harmless no-op.
    fn migrate(&self) -> rusqlite::Result<()> {
        let version: i64 = self.conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version < 1 {
            // v1: the file <-> page mapping table.
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS nodes (
                  rel_path             TEXT PRIMARY KEY,
                  kind                 TEXT NOT NULL,
                  notion_page_id       TEXT NOT NULL,
                  parent_page_id       TEXT NOT NULL,
                  content_hash         TEXT,
                  body_block_ids       TEXT NOT NULL DEFAULT '[]',
                  local_mtime_ns       INTEGER,
                  notion_last_edited   TEXT,
                  last_synced_dir      TEXT,
                  is_binary_placeholder INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_nodes_hash ON nodes(content_hash);
                CREATE INDEX IF NOT EXISTS idx_nodes_page ON nodes(notion_page_id);
                "#,
            )?;
        }

        if version < 2 {
            // v2: content-addressed snapshot index + append-only sync journal. The
            // blobs themselves live on disk in the object store; `snapshot` is the
            // index, `journal` is the audit trail that makes a restore trustworthy.
            self.conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS snapshot (
                  id          INTEGER PRIMARY KEY,
                  rel_path    TEXT    NOT NULL,
                  page_id     TEXT,
                  side        TEXT    NOT NULL CHECK (side IN ('local','notion')),
                  blake3      TEXT    NOT NULL,
                  size_bytes  INTEGER NOT NULL,
                  reason      TEXT    NOT NULL,
                  captured_at TEXT    NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_snapshot_path ON snapshot(rel_path, captured_at DESC);
                CREATE INDEX IF NOT EXISTS idx_snapshot_hash ON snapshot(blake3);

                CREATE TABLE IF NOT EXISTS journal (
                  id        INTEGER PRIMARY KEY,
                  ts        TEXT NOT NULL,
                  rel_path  TEXT NOT NULL,
                  action    TEXT NOT NULL,
                  from_hash TEXT,
                  to_hash   TEXT,
                  side      TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS idx_journal_path ON journal(rel_path, id DESC);
                "#,
            )?;
        }

        // Record the schema version we just converged to.
        self.conn.pragma_update(None, "user_version", 2i64)?;
        Ok(())
    }

    pub fn upsert(&self, node: &Node) -> rusqlite::Result<()> {
        let body = serde_json::to_string(&node.body_block_ids).unwrap_or_else(|_| "[]".into());
        self.conn.execute(
            r#"
            INSERT INTO nodes (rel_path, kind, notion_page_id, parent_page_id, content_hash,
                               body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir,
                               is_binary_placeholder)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
            ON CONFLICT(rel_path) DO UPDATE SET
              kind=excluded.kind,
              notion_page_id=excluded.notion_page_id,
              parent_page_id=excluded.parent_page_id,
              content_hash=excluded.content_hash,
              body_block_ids=excluded.body_block_ids,
              local_mtime_ns=excluded.local_mtime_ns,
              notion_last_edited=excluded.notion_last_edited,
              last_synced_dir=excluded.last_synced_dir,
              is_binary_placeholder=excluded.is_binary_placeholder
            "#,
            params![
                node.rel_path,
                node.kind.as_str(),
                node.notion_page_id,
                node.parent_page_id,
                node.content_hash,
                body,
                node.local_mtime_ns,
                node.notion_last_edited,
                node.last_synced_dir,
                node.is_binary_placeholder as i64,
            ],
        )?;
        Ok(())
    }

    pub fn get_by_path(&self, rel_path: &str) -> rusqlite::Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
                 body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
                 is_binary_placeholder FROM nodes WHERE rel_path = ?1",
                params![rel_path],
                row_to_node,
            )
            .optional()
    }

    /// Find a file node by content hash (powers rename detection).
    pub fn get_by_hash(&self, hash: &str) -> rusqlite::Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
                 body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
                 is_binary_placeholder FROM nodes WHERE content_hash = ?1 AND kind = 'file' LIMIT 1",
                params![hash],
                row_to_node,
            )
            .optional()
    }

    pub fn get_by_page_id(&self, page_id: &str) -> rusqlite::Result<Option<Node>> {
        self.conn
            .query_row(
                "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
                 body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
                 is_binary_placeholder FROM nodes WHERE notion_page_id = ?1",
                params![page_id],
                row_to_node,
            )
            .optional()
    }

    pub fn all_tracked(&self) -> rusqlite::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
             body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
             is_binary_placeholder FROM nodes",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        rows.collect()
    }

    pub fn delete(&self, rel_path: &str) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM nodes WHERE rel_path = ?1", params![rel_path])?;
        Ok(())
    }

    /// Move a node to a new rel_path (rename), preserving its page id and history.
    pub fn rename_path(&self, old: &str, new: &str, new_parent: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE nodes SET rel_path = ?2, parent_page_id = ?3 WHERE rel_path = ?1",
            params![old, new, new_parent],
        )?;
        Ok(())
    }

    /// File nodes that participate in the poll loop. Filtering in SQL keeps the poller
    /// from materializing dir rows and binary placeholders it would only skip.
    pub fn tracked_files(&self) -> rusqlite::Result<Vec<Node>> {
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
             body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
             is_binary_placeholder FROM nodes \
             WHERE kind = 'file' AND is_binary_placeholder = 0",
        )?;
        let rows = stmt.query_map([], row_to_node)?;
        rows.collect()
    }

    /// Delete a node and, if it is a directory, every descendant row. Trashing a dir
    /// page in Notion trashes its whole subtree, so the matching rows must go too or
    /// they become orphans pointing at trashed pages. Returns the removed rows so the
    /// caller can trash pages / drop locks. Uses substr() rather than LIKE so '_' and
    /// '%' inside a rel_path are never treated as wildcards.
    pub fn delete_subtree(&self, rel_path: &str) -> rusqlite::Result<Vec<Node>> {
        let prefix = format!("{rel_path}/");
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, kind, notion_page_id, parent_page_id, content_hash, \
             body_block_ids, local_mtime_ns, notion_last_edited, last_synced_dir, \
             is_binary_placeholder FROM nodes \
             WHERE rel_path = ?1 OR substr(rel_path, 1, length(?2)) = ?2",
        )?;
        let removed: Vec<Node> = stmt
            .query_map(params![rel_path, prefix], row_to_node)?
            .collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        self.conn.execute(
            "DELETE FROM nodes WHERE rel_path = ?1 OR substr(rel_path, 1, length(?2)) = ?2",
            params![rel_path, prefix],
        )?;
        Ok(removed)
    }

    // --- snapshot index (v2 schema) -------------------------------------------

    /// Record a snapshot row. The bytes themselves live in the content-addressed
    /// object store keyed by `blake3`; this is only the index entry.
    pub fn insert_snapshot(
        &self,
        rel_path: &str,
        page_id: Option<&str>,
        side: &str,
        blake3: &str,
        size_bytes: i64,
        reason: &str,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO snapshot (rel_path, page_id, side, blake3, size_bytes, reason, captured_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![rel_path, page_id, side, blake3, size_bytes, reason, now_rfc3339()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn list_snapshots(&self, rel_path: &str, limit: usize) -> rusqlite::Result<Vec<SnapshotRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rel_path, page_id, side, blake3, size_bytes, reason, captured_at \
             FROM snapshot WHERE rel_path = ?1 ORDER BY captured_at DESC, id DESC LIMIT ?2",
        )?;
        stmt.query_map(params![rel_path, limit as i64], row_to_snapshot)?
            .collect()
    }

    pub fn snapshot_by_id(&self, id: i64) -> rusqlite::Result<Option<SnapshotRow>> {
        self.conn
            .query_row(
                "SELECT id, rel_path, page_id, side, blake3, size_bytes, reason, captured_at \
                 FROM snapshot WHERE id = ?1",
                params![id],
                row_to_snapshot,
            )
            .optional()
    }

    /// Newest snapshot for `rel_path` captured at or before `cutoff` (RFC3339).
    pub fn snapshot_at_or_before(
        &self,
        rel_path: &str,
        cutoff: &str,
    ) -> rusqlite::Result<Option<SnapshotRow>> {
        self.conn
            .query_row(
                "SELECT id, rel_path, page_id, side, blake3, size_bytes, reason, captured_at \
                 FROM snapshot WHERE rel_path = ?1 AND captured_at <= ?2 \
                 ORDER BY captured_at DESC, id DESC LIMIT 1",
                params![rel_path, cutoff],
                row_to_snapshot,
            )
            .optional()
    }

    /// Distinct blob hashes still referenced by the index (GC mark phase).
    pub fn distinct_snapshot_hashes(&self) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut stmt = self.conn.prepare("SELECT DISTINCT blake3 FROM snapshot")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?.collect()
    }

    /// Drop snapshot rows older than `cutoff` (RFC3339) while always keeping the newest
    /// `keep_min` per (rel_path, side). Needs SQLite >= 3.25 for row_number() OVER;
    /// the bundled build satisfies that. Returns the number of rows removed.
    pub fn gc_snapshots(&mut self, cutoff: &str, keep_min: u32) -> rusqlite::Result<usize> {
        let tx = self.conn.transaction()?;
        let removed = tx.execute(
            "DELETE FROM snapshot \
              WHERE captured_at < ?1 \
                AND id NOT IN ( \
                  SELECT id FROM ( \
                    SELECT id, row_number() OVER ( \
                             PARTITION BY rel_path, side ORDER BY captured_at DESC) AS rn \
                    FROM snapshot) \
                  WHERE rn <= ?2)",
            params![cutoff, keep_min],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    // --- append-only sync journal (v2 schema) ---------------------------------

    /// Append a journal row describing one sync action: the audit trail that makes a
    /// restore trustworthy ("what clobbered this file at 2am?").
    pub fn insert_journal(
        &self,
        rel_path: &str,
        action: &str,
        from_hash: Option<&str>,
        to_hash: Option<&str>,
        side: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO journal (ts, rel_path, action, from_hash, to_hash, side) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![now_rfc3339(), rel_path, action, from_hash, to_hash, side],
        )?;
        Ok(())
    }

    pub fn list_journal(
        &self,
        rel_path: Option<&str>,
        limit: usize,
    ) -> rusqlite::Result<Vec<JournalRow>> {
        match rel_path {
            Some(p) => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, ts, rel_path, action, from_hash, to_hash, side FROM journal \
                     WHERE rel_path = ?1 ORDER BY id DESC LIMIT ?2",
                )?;
                stmt.query_map(params![p, limit as i64], row_to_journal)?
                    .collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT id, ts, rel_path, action, from_hash, to_hash, side FROM journal \
                     ORDER BY id DESC LIMIT ?1",
                )?;
                stmt.query_map(params![limit as i64], row_to_journal)?.collect()
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct SnapshotRow {
    pub id: i64,
    pub rel_path: String,
    pub page_id: Option<String>,
    pub side: String,
    pub blake3: String,
    pub size_bytes: i64,
    pub reason: String,
    pub captured_at: String,
}

#[derive(Debug, Clone)]
pub struct JournalRow {
    pub id: i64,
    pub ts: String,
    pub rel_path: String,
    pub action: String,
    pub from_hash: Option<String>,
    pub to_hash: Option<String>,
    pub side: String,
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotRow> {
    Ok(SnapshotRow {
        id: row.get(0)?,
        rel_path: row.get(1)?,
        page_id: row.get(2)?,
        side: row.get(3)?,
        blake3: row.get(4)?,
        size_bytes: row.get(5)?,
        reason: row.get(6)?,
        captured_at: row.get(7)?,
    })
}

fn row_to_journal(row: &rusqlite::Row<'_>) -> rusqlite::Result<JournalRow> {
    Ok(JournalRow {
        id: row.get(0)?,
        ts: row.get(1)?,
        rel_path: row.get(2)?,
        action: row.get(3)?,
        from_hash: row.get(4)?,
        to_hash: row.get(5)?,
        side: row.get(6)?,
    })
}

/// RFC3339 UTC timestamp (millisecond precision), dependency-free.
pub fn now_rfc3339() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format_rfc3339(dur.as_secs(), dur.subsec_millis())
}

/// RFC3339 UTC timestamp for (now - `secs`). Used for GC cutoffs and `--at` ages.
pub fn rfc3339_minus_secs(secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_rfc3339(now.saturating_sub(secs), 0)
}

fn format_rfc3339(secs: u64, millis: u32) -> String {
    let days = (secs / 86_400) as i64;
    let rem = (secs % 86_400) as u32;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
}

// days since 1970-01-01 -> (year, month, day). Howard Hinnant's civil-from-days.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn row_to_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let kind: String = row.get(1)?;
    let body_json: String = row.get(5)?;
    let body_block_ids: Vec<String> = serde_json::from_str(&body_json).unwrap_or_default();
    let bin: i64 = row.get(9)?;
    Ok(Node {
        rel_path: row.get(0)?,
        kind: NodeKind::parse(&kind),
        notion_page_id: row.get(2)?,
        parent_page_id: row.get(3)?,
        content_hash: row.get(4)?,
        body_block_ids,
        local_mtime_ns: row.get(6)?,
        notion_last_edited: row.get(7)?,
        last_synced_dir: row.get(8)?,
        is_binary_placeholder: bin != 0,
    })
}

fn default_db_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/state")
        });
    base.join("notion-sync").join("state.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(path: &str, hash: &str) -> Node {
        Node {
            rel_path: path.into(),
            kind: NodeKind::File,
            notion_page_id: format!("page-{path}"),
            parent_page_id: "root".into(),
            content_hash: Some(hash.into()),
            body_block_ids: vec!["b1".into(), "b2".into()],
            local_mtime_ns: Some(42),
            notion_last_edited: Some("2026-01-01T00:00:00.000Z".into()),
            last_synced_dir: Some("to_notion".into()),
            is_binary_placeholder: false,
        }
    }

    #[test]
    fn upsert_and_get() {
        let st = State::open_in_memory().unwrap();
        st.upsert(&sample("src/main.rs", "hashA")).unwrap();
        let got = st.get_by_path("src/main.rs").unwrap().unwrap();
        assert_eq!(got.notion_page_id, "page-src/main.rs");
        assert_eq!(got.body_block_ids, vec!["b1", "b2"]);
    }

    #[test]
    fn get_by_hash_powers_rename() {
        let st = State::open_in_memory().unwrap();
        st.upsert(&sample("src/old.rs", "H")).unwrap();
        let found = st.get_by_hash("H").unwrap().unwrap();
        assert_eq!(found.rel_path, "src/old.rs");
    }

    #[test]
    fn rename_preserves_page_id() {
        let st = State::open_in_memory().unwrap();
        st.upsert(&sample("a.rs", "H")).unwrap();
        let page_before = st.get_by_path("a.rs").unwrap().unwrap().notion_page_id;
        st.rename_path("a.rs", "b.rs", "root").unwrap();
        assert!(st.get_by_path("a.rs").unwrap().is_none());
        let after = st.get_by_path("b.rs").unwrap().unwrap();
        assert_eq!(after.notion_page_id, page_before);
    }
}

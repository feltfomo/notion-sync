//! SQLite-backed sync state (`state.db`).
//!
//! NOTE (multi-machine limitation, v1): this database is intentionally machine-local.
//! Running two daemons against the same Notion tree from different machines is
//! UNSUPPORTED in v1 — there is no shared/remote state. Do not design around it.

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
        if s == "dir" { NodeKind::Dir } else { NodeKind::File }
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

    fn migrate(&self) -> rusqlite::Result<()> {
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
        )
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
        self.conn.execute("DELETE FROM nodes WHERE rel_path = ?1", params![rel_path])?;
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
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
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

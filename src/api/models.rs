//! Serde models covering just the slice of the Notion API this daemon touches.
//!
//! Only the fields we read or write are here. serde drops unknown fields on
//! deserialize, so these partial structs are safe.

use serde::{Deserialize, Serialize};

// content length is bounded by Notion's per-item limit (UTF-16 units; see chunk.rs).
#[derive(Debug, Serialize)]
pub struct RichTextReq {
    #[serde(rename = "type")]
    pub ty: &'static str, // always "text"
    pub text: TextContentReq,
}

#[derive(Debug, Serialize)]
pub struct TextContentReq {
    pub content: String,
}

impl RichTextReq {
    pub fn text(content: String) -> Self {
        RichTextReq {
            ty: "text",
            text: TextContentReq { content },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct CodeBlockReq {
    pub object: &'static str, // "block"
    #[serde(rename = "type")]
    pub ty: &'static str, // "code"
    pub code: CodeReq,
}

#[derive(Debug, Serialize)]
pub struct CodeReq {
    pub rich_text: Vec<RichTextReq>,
    pub language: String,
}

impl CodeBlockReq {
    pub fn new(language: String, rich_text: Vec<RichTextReq>) -> Self {
        CodeBlockReq {
            object: "block",
            ty: "code",
            code: CodeReq {
                rich_text,
                language,
            },
        }
    }
}

/// A callout block (used for the binary-file placeholder).
#[derive(Debug, Serialize)]
pub struct CalloutBlockReq {
    pub object: &'static str,
    #[serde(rename = "type")]
    pub ty: &'static str, // "callout"
    pub callout: CalloutReq,
}

#[derive(Debug, Serialize)]
pub struct CalloutReq {
    pub rich_text: Vec<RichTextReq>,
    pub icon: IconReq,
}

#[derive(Debug, Serialize)]
pub struct IconReq {
    #[serde(rename = "type")]
    pub ty: &'static str, // "emoji"
    pub emoji: String,
}

impl CalloutBlockReq {
    pub fn warning(message: String) -> Self {
        CalloutBlockReq {
            object: "block",
            ty: "callout",
            callout: CalloutReq {
                rich_text: vec![RichTextReq::text(message)],
                icon: IconReq {
                    ty: "emoji",
                    emoji: "\u{26A0}\u{FE0F}".to_string(),
                },
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AppendChildrenReq {
    pub children: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct CreatePageReq {
    pub parent: ParentReq,
    pub properties: serde_json::Value,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<serde_json::Value>,
}

#[derive(Debug, Serialize)]
pub struct ParentReq {
    #[serde(rename = "type")]
    pub ty: &'static str, // "page_id"
    pub page_id: String,
}

impl ParentReq {
    pub fn page(page_id: String) -> Self {
        ParentReq {
            ty: "page_id",
            page_id,
        }
    }
}

/// Build the `properties` object for a title-only page (pages parented by a page
/// only accept the title property).
pub fn title_properties(title: &str) -> serde_json::Value {
    serde_json::json!({
        "title": { "title": [ { "text": { "content": title } } ] }
    })
}

#[derive(Debug, Deserialize)]
pub struct PageResp {
    pub id: String,
    pub last_edited_time: String,
    #[serde(default)]
    pub last_edited_by: Option<PartialUser>,
    /// `in_trash` is the trash flag on `2022-06-28` (added Apr 2024) and later.
    #[serde(default)]
    pub in_trash: bool,
    /// Older readback field; some API versions still echo `archived` alongside (or
    /// instead of) `in_trash`. Treated as equivalent for trash detection.
    #[serde(default)]
    pub archived: Option<bool>,
    /// The page's parent. Remote-first discovery walks this back to a mapping root to
    /// place an untracked page on disk. Calls that don't need it just ignore it.
    #[serde(default)]
    pub parent: Option<ParentResp>,
    /// Page properties, projected to just the title (the only property a page-parented
    /// page carries). Used to derive a discovered page's filename.
    #[serde(default)]
    pub properties: Option<PageProperties>,
}

impl PageResp {
    /// True if the page is trashed/archived, tolerant of which field the pinned API
    /// version actually populates.
    pub fn trashed(&self) -> bool {
        self.in_trash || self.archived.unwrap_or(false)
    }

    /// The parent page id, but only when the parent is actually another page. A
    /// workspace/database/data_source parent returns None: those aren't part of any
    /// file-tree mapping, so a page under one can't be placed on disk.
    pub fn parent_page_id(&self) -> Option<&str> {
        let p = self.parent.as_ref()?;
        if p.ty == "page_id" {
            p.page_id.as_deref()
        } else {
            None
        }
    }

    /// The page title as plain text (title rich-text runs concatenated). Empty when the
    /// title is unset or wasn't projected. This is the inverse of `util::title_for`,
    /// which names every file page after its filename, so the title IS the filename.
    pub fn title(&self) -> String {
        let Some(props) = &self.properties else {
            return String::new();
        };
        let Some(t) = &props.title else {
            return String::new();
        };
        t.title.iter().map(|r| r.plain_text.as_str()).collect()
    }
}

/// Minimal projection of a page `parent` object. Only the page-parent case carries an
/// id we can act on; the `type` discriminates it from workspace/database parents.
#[derive(Debug, Deserialize)]
pub struct ParentResp {
    #[serde(rename = "type", default)]
    pub ty: String,
    #[serde(default)]
    pub page_id: Option<String>,
}

/// Page `properties`, projected to just the title. A page parented by another page only
/// carries a title property (always keyed "title"), so that's all we model.
#[derive(Debug, Deserialize)]
pub struct PageProperties {
    #[serde(default)]
    pub title: Option<TitleProp>,
}

#[derive(Debug, Deserialize)]
pub struct TitleProp {
    #[serde(default)]
    pub title: Vec<RichTextResp>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct PartialUser {
    pub id: String,
}

/// Result of GET /v1/users/me (to learn our own bot user id for echo suppression).
#[derive(Debug, Deserialize)]
pub struct MeResp {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct BlockResp {
    pub id: String,
    #[serde(rename = "type")]
    pub ty: String,
    pub last_edited_time: String,
    #[serde(default)]
    pub code: Option<CodeResp>,
    /// Present when `ty == "child_page"`; used by reconciliation to adopt pages.
    #[serde(default)]
    pub child_page: Option<ChildPageResp>,
}

#[derive(Debug, Deserialize)]
pub struct ChildPageResp {
    #[serde(default)]
    pub title: String,
}

#[derive(Debug, Deserialize)]
pub struct CodeResp {
    #[serde(default)]
    pub rich_text: Vec<RichTextResp>,
    #[serde(default)]
    pub language: String,
}

#[derive(Debug, Deserialize)]
pub struct RichTextResp {
    /// The authoritative readback text for fidelity reassembly.
    #[serde(default)]
    pub plain_text: String,
}

#[derive(Debug, Deserialize)]
pub struct ChildrenListResp {
    pub results: Vec<BlockResp>,
    #[serde(default)]
    pub next_cursor: Option<String>,
    #[serde(default)]
    pub has_more: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_concatenates_rich_text_runs_and_reads_page_parent() {
        let page: PageResp = serde_json::from_value(serde_json::json!({
            "id": "p1",
            "last_edited_time": "t",
            "parent": { "type": "page_id", "page_id": "parent-1" },
            "properties": { "title": { "title": [
                { "plain_text": "main" }, { "plain_text": ".rs" }
            ] } }
        }))
        .unwrap();
        assert_eq!(page.title(), "main.rs");
        assert_eq!(page.parent_page_id(), Some("parent-1"));
    }

    #[test]
    fn non_page_parent_and_missing_title_are_none_and_empty() {
        // A workspace-rooted page has no page parent to place under, and a page with no
        // projected title yields an empty filename (rejected downstream).
        let page: PageResp = serde_json::from_value(serde_json::json!({
            "id": "p2",
            "last_edited_time": "t",
            "parent": { "type": "workspace", "workspace": true }
        }))
        .unwrap();
        assert_eq!(page.parent_page_id(), None);
        assert_eq!(page.title(), "");
    }
}

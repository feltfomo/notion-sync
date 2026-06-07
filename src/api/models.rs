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
    #[serde(default)]
    pub in_trash: bool,
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

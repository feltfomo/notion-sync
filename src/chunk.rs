//! Positional chunker: split UTF-8 into rich-text segments and reassemble them
//! byte-for-byte. Order is the only structure; sizing is bounded by Notion's
//! per-item limit (see `split_into_segments`).

use crate::api::models::{CodeBlockReq, RichTextReq};

pub const MAX_CHARS_PER_ITEM: usize = 2000;
pub const MAX_ITEMS_PER_BLOCK: usize = 100;
pub const MAX_CHILDREN_PER_REQUEST: usize = 100;

pub struct EncodedBlock {
    // &'static: language::for_path returns a static table entry.
    pub language: &'static str,
    pub segments: Vec<String>,
}

impl EncodedBlock {
    // Consumes self: the body is sent once, so move segments instead of cloning.
    pub fn into_json(self) -> serde_json::Value {
        let rich: Vec<RichTextReq> = self.segments.into_iter().map(RichTextReq::text).collect();
        let block = CodeBlockReq::new(self.language.to_string(), rich);
        serde_json::to_value(block).expect("code block serializes")
    }
}

// Empty files still emit one empty block, so every page has a body block to diff against.
pub fn encode(content: &str, language: &'static str) -> Vec<EncodedBlock> {
    let segments = split_into_segments(content);
    let segments = if segments.is_empty() {
        vec![String::new()]
    } else {
        segments
    };

    // Move segments in rather than chunks().to_vec(), which would clone each one.
    let mut blocks = Vec::new();
    let mut current = Vec::with_capacity(MAX_ITEMS_PER_BLOCK);
    for seg in segments {
        current.push(seg);
        if current.len() == MAX_ITEMS_PER_BLOCK {
            blocks.push(EncodedBlock {
                language,
                segments: std::mem::take(&mut current),
            });
        }
    }
    if !current.is_empty() {
        blocks.push(EncodedBlock {
            language,
            segments: current,
        });
    }
    blocks
}

// By value: the caller drops these right after, so move into JSON instead of cloning.
pub fn batch_blocks(blocks: Vec<EncodedBlock>) -> Vec<Vec<serde_json::Value>> {
    let mut batches = Vec::new();
    let mut current = Vec::with_capacity(MAX_CHILDREN_PER_REQUEST);
    for block in blocks {
        current.push(block.into_json());
        if current.len() == MAX_CHILDREN_PER_REQUEST {
            batches.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        batches.push(current);
    }
    batches
}

pub fn reassemble(blocks_plain_text: &[Vec<String>]) -> String {
    let mut out = String::new();
    for block in blocks_plain_text {
        for seg in block {
            out.push_str(seg);
        }
    }
    out
}

// Notion counts length in UTF-16 units, not scalars, so a non-BMP char costs 2. Flush
// before overflowing and never split a scalar, or write-back corrupts.
fn split_into_segments(content: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for ch in content.chars() {
        let w = ch.len_utf16();
        if units + w > MAX_CHARS_PER_ITEM && !current.is_empty() {
            segments.push(std::mem::take(&mut current));
            units = 0;
        }
        current.push(ch);
        units += w;
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(content: &str) -> String {
        let blocks = encode(content, "rust");
        // Simulate a lossless Notion readback: plain_text == what we sent.
        let readback: Vec<Vec<String>> = blocks.iter().map(|b| b.segments.clone()).collect();
        reassemble(&readback)
    }

    #[test]
    fn empty_file_roundtrips() {
        assert_eq!(roundtrip(""), "");
        assert_eq!(encode("", "rust").len(), 1);
    }

    #[test]
    fn small_file_roundtrips() {
        let s = "fn main() {\n\tprintln!(\"hi\");\n}\n";
        assert_eq!(roundtrip(s), s);
    }

    #[test]
    fn trailing_whitespace_and_tabs_preserved() {
        let s = "a   \n\tb\n\t\tc\n";
        assert_eq!(roundtrip(s), s);
    }

    #[test]
    fn long_line_over_2000_chars_splits_and_rejoins() {
        let s = "x".repeat(5001);
        let blocks = encode(&s, "rust");
        assert_eq!(blocks[0].segments.len(), 3); // 2000 + 2000 + 1001 within one block
        assert_eq!(roundtrip(&s), s);
    }

    #[test]
    fn multibyte_chars_never_split() {
        // 2001 emoji => crosses the 2000 boundary; must not corrupt the boundary char.
        let s = "\u{1F680}".repeat(2001);
        assert_eq!(roundtrip(&s), s);
    }

    #[test]
    fn over_100_blocks_batches_correctly() {
        // 101 full blocks + remainder => 102 blocks, batched into 100 + 2.
        let s = "y".repeat(MAX_CHARS_PER_ITEM * MAX_ITEMS_PER_BLOCK * 101 + 5);
        let blocks = encode(&s, "rust");
        assert_eq!(blocks.len(), 102);
        let batches = batch_blocks(encode(&s, "rust"));
        assert_eq!(batches.len(), 2); // 100 + 2
        assert!(batches.iter().all(|b| b.len() <= MAX_CHILDREN_PER_REQUEST));
        assert_eq!(roundtrip(&s), s);
    }

    #[test]
    fn empty_file_emits_one_block_through_batching() {
        // An empty file still yields one block with a single empty segment, so every
        // page keeps a body block to diff against.
        let blocks = encode("", "rust");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].segments, vec![String::new()]);
        let batches = batch_blocks(encode("", "rust"));
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].len(), 1);
    }
}

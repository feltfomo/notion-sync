//! Positional chunker: split UTF-8 into rich-text segments and reassemble them
//! byte-for-byte. Order is the only structure; sizing is bounded by Notion's
//! per-item limit (see `split_into_segments`).

use crate::api::models::{CodeBlockReq, RichTextReq};

pub const MAX_CHARS_PER_ITEM: usize = 2000;
pub const MAX_ITEMS_PER_BLOCK: usize = 100;
pub const MAX_CHILDREN_PER_REQUEST: usize = 100;

pub struct EncodedBlock {
    pub language: String,
    pub segments: Vec<String>,
}

impl EncodedBlock {
    pub fn to_json(&self) -> serde_json::Value {
        let rich: Vec<RichTextReq> = self
            .segments
            .iter()
            .map(|s| RichTextReq::text(s.clone()))
            .collect();
        let block = CodeBlockReq::new(self.language.clone(), rich);
        serde_json::to_value(block).expect("code block serializes")
    }
}

// Empty files still emit one empty block, so every page has a body block to diff against.
pub fn encode(content: &str, language: &str) -> Vec<EncodedBlock> {
    let segments = split_into_segments(content);
    let segments = if segments.is_empty() { vec![String::new()] } else { segments };

    let mut blocks = Vec::new();
    for chunk in segments.chunks(MAX_ITEMS_PER_BLOCK) {
        blocks.push(EncodedBlock {
            language: language.to_string(),
            segments: chunk.to_vec(),
        });
    }
    blocks
}

pub fn batch_blocks(blocks: &[EncodedBlock]) -> Vec<Vec<serde_json::Value>> {
    blocks
        .chunks(MAX_CHILDREN_PER_REQUEST)
        .map(|grp| grp.iter().map(|b| b.to_json()).collect())
        .collect()
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

// Notion counts content length in UTF-16 units (JS semantics), not scalars, so a
// non-BMP char costs 2. Flush before overflowing the budget and never split a scalar,
// or surrogate pairs break and write-back corrupts. The fidelity probe exists to catch this.
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
        let batches = batch_blocks(&blocks);
        assert_eq!(batches.len(), 2); // 100 + 2
        assert!(batches.iter().all(|b| b.len() <= MAX_CHILDREN_PER_REQUEST));
        assert_eq!(roundtrip(&s), s);
    }
}

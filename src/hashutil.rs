//! Content hashing (blake3). Used for change detection and rename matching.

pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub fn hash_str(s: &str) -> String {
    hash_bytes(s.as_bytes())
}

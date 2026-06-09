//! Filesystem helpers: binary sniffing, atomic writes, directory walking.

use std::path::{Path, PathBuf};

/// The outcome of sniffing a file for the text-mirror path: either decoded UTF-8 text
/// ready to mirror, or the original bytes handed back for a binary placeholder.
pub enum TextOrBinary {
    Text(String),
    Binary(Vec<u8>),
}

/// Sniff and decode in a single pass. v1 mirrors text (UTF-8) only, so a file is binary
/// if it has a NUL byte in the first 8 KiB OR is not valid UTF-8. The NUL check runs
/// first and short-circuits (a NUL wins before any UTF-8 scan). On the text path the
/// UTF-8 validation also yields the String; on the binary path the bytes are returned
/// intact for the placeholder.
pub fn classify_text(bytes: Vec<u8>) -> TextOrBinary {
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return TextOrBinary::Binary(bytes);
    }
    match String::from_utf8(bytes) {
        Ok(text) => TextOrBinary::Text(text),
        Err(e) => TextOrBinary::Binary(e.into_bytes()),
    }
}

// temp + fsync + rename: a crash mid-write can't leave a partial file in place.
// Blocking; from the async runtime use `atomic_write` (below) so the worker isn't stalled.
pub fn atomic_write_blocking(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("out"),
        std::process::id()
    ));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Async wrapper: routes the blocking atomic write through spawn_blocking so disk I/O
/// never stalls an async worker thread.
pub async fn atomic_write(path: &Path, bytes: Vec<u8>) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || atomic_write_blocking(&path, &bytes))
        .await
        .map_err(std::io::Error::other)?
}

pub struct WalkEntry {
    pub rel_path: String,
    pub abs_path: PathBuf,
    pub is_dir: bool,
}

// Yields parents before children so directory pages exist before their contents.
// Private: the only entry point is the async `walk_async` wrapper below.
fn walk(root: &Path, ignore: &[String]) -> std::io::Result<Vec<WalkEntry>> {
    let mut out = Vec::new();
    walk_inner(root, root, ignore, &mut out)?;
    Ok(out)
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    ignore: &[String],
    out: &mut Vec<WalkEntry>,
) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let abs = entry.path();
        let rel = match abs.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if crate::config::is_ignored(&rel, ignore) {
            continue;
        }
        let rel_str = rel_to_unix(&rel);
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            out.push(WalkEntry {
                rel_path: rel_str,
                abs_path: abs.clone(),
                is_dir: true,
            });
            walk_inner(root, &abs, ignore, out)?;
        } else if file_type.is_file() {
            out.push(WalkEntry {
                rel_path: rel_str,
                abs_path: abs,
                is_dir: false,
            });
        }
        // Symlinks are skipped in v1.
    }
    Ok(())
}

pub fn rel_to_unix(rel: &Path) -> String {
    rel.components()
        .filter_map(|c| c.as_os_str().to_str())
        .collect::<Vec<_>>()
        .join("/")
}

/// The page title for a node is its file/dir name (the last path segment).
pub fn title_for(rel_path: &str) -> &str {
    rel_path.rsplit('/').next().unwrap_or(rel_path)
}

/// The parent rel_path of a node ("" means the mapping root).
pub fn parent_rel(rel_path: &str) -> String {
    match rel_path.rsplit_once('/') {
        Some((parent, _)) => parent.to_string(),
        None => String::new(),
    }
}

/// Async wrapper around `walk`: the recursive read_dir is blocking, so run it on the
/// blocking pool to keep the reconcile pass off the async worker threads.
pub async fn walk_async(root: PathBuf, ignore: Vec<String>) -> std::io::Result<Vec<WalkEntry>> {
    tokio::task::spawn_blocking(move || walk(&root, &ignore))
        .await
        .map_err(std::io::Error::other)?
}

/// File mtime in nanoseconds since the Unix epoch (a cheap local-change hint).
pub fn file_mtime_ns(path: &Path) -> Option<i64> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?;
    let dur = mtime.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_nanos() as i64)
}

#[cfg(test)]
mod tests {
    use super::{classify_text, TextOrBinary};

    #[test]
    fn utf8_text_decodes_to_string() {
        match classify_text(b"fn main() {}\n".to_vec()) {
            TextOrBinary::Text(s) => assert_eq!(s, "fn main() {}\n"),
            TextOrBinary::Binary(_) => panic!("ascii should be text"),
        }
        // Multibyte UTF-8 survives intact.
        match classify_text("café — π".as_bytes().to_vec()) {
            TextOrBinary::Text(s) => assert_eq!(s, "café — π"),
            TextOrBinary::Binary(_) => panic!("valid utf-8 should be text"),
        }
        // Empty input is empty text, not binary.
        match classify_text(Vec::new()) {
            TextOrBinary::Text(s) => assert!(s.is_empty()),
            TextOrBinary::Binary(_) => panic!("empty should be text"),
        }
    }

    #[test]
    fn nul_in_head_is_binary_even_though_nul_is_valid_utf8() {
        // A NUL byte is itself valid UTF-8, so this pins the precedence: the NUL check
        // must win before the UTF-8 scan would accept the buffer as text.
        let mut bytes = b"some text".to_vec();
        bytes.push(0);
        bytes.extend_from_slice(b"more text");
        let expected = bytes.clone();
        match classify_text(bytes) {
            TextOrBinary::Binary(b) => {
                assert_eq!(b, expected, "bytes returned intact for the placeholder")
            }
            TextOrBinary::Text(_) => panic!("NUL in head must classify as binary"),
        }
    }

    #[test]
    fn invalid_utf8_is_binary_and_returns_bytes_intact() {
        let raw = vec![0xff, 0xfe, 0xfd];
        match classify_text(raw.clone()) {
            TextOrBinary::Binary(b) => assert_eq!(b, raw),
            TextOrBinary::Text(_) => panic!("invalid utf-8 must classify as binary"),
        }
    }

    #[test]
    fn nul_past_the_first_8kib_is_not_treated_as_binary() {
        // Only the first 8 KiB are sniffed for NUL; a NUL beyond that, in otherwise
        // valid UTF-8, stays text (NUL is valid UTF-8). Locks the sniff-window size.
        let mut bytes = vec![b'a'; 8192];
        bytes.push(0);
        match classify_text(bytes) {
            TextOrBinary::Text(s) => assert_eq!(s.len(), 8193),
            TextOrBinary::Binary(_) => panic!("NUL past the head window should stay text"),
        }
    }
}

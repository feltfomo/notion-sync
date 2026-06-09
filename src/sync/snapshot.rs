//! Content-addressed snapshot object store. Blobs are gzip-compressed and keyed by
//! the blake3 hash of their *uncompressed* bytes, so identical content is stored
//! exactly once no matter how many snapshots reference it. Git-style fanout keeps any
//! single directory small:
//!
//!   <local_root>/.notion-sync/objects/ab/cd/<full-hash>.gz
//!
//! All filesystem work here is synchronous. Callers on the async runtime must use the
//! async wrappers (`put` / `get`), which hop onto the blocking pool so a worker thread
//! is never stalled on disk I/O.

use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::hashutil;

#[derive(Clone)]
pub struct ObjectStore {
    objects_dir: PathBuf,
}

impl ObjectStore {
    /// Objects live under `<local_root>/.notion-sync/objects`. Retained for tests and
    /// for importing a pre-multi-directory install's per-root store.
    pub fn new(local_root: &Path) -> Self {
        ObjectStore {
            objects_dir: local_root.join(".notion-sync").join("objects"),
        }
    }

    /// The shared daemon store, under the state dir (alongside state.db) so it is
    /// independent of any single mapping's root. With several mappings there is no one
    /// root to hang the store off, and a shared store keeps content-addressed dedup
    /// working across all of them.
    pub fn open_default() -> Self {
        ObjectStore {
            objects_dir: default_objects_dir(),
        }
    }

    /// A store rooted at an explicit objects directory (tests, and the legacy import).
    pub fn at(objects_dir: PathBuf) -> Self {
        ObjectStore { objects_dir }
    }

    /// The directory this store reads and writes blobs under.
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Move every blob from a legacy per-root store (`<old_root>/.notion-sync/objects`)
    /// into this store, preserving the git-style fanout. Used once when migrating a
    /// pre-multi-directory install to the shared store so old snapshots stay readable.
    /// Best-effort per blob; returns how many were moved. A blob already present here
    /// (content-addressed, so identical) is dropped from the old store rather than
    /// recopied.
    pub fn import_legacy_root_store(&self, old_root: &Path) -> std::io::Result<usize> {
        let old = old_root.join(".notion-sync").join("objects");
        if !old.is_dir() || old == self.objects_dir {
            return Ok(0);
        }
        let mut moved = 0usize;
        for a in read_subdirs(&old)? {
            let Some(a_name) = a.file_name().map(|s| s.to_owned()) else {
                continue;
            };
            for b in read_subdirs(&a)? {
                let Some(b_name) = b.file_name().map(|s| s.to_owned()) else {
                    continue;
                };
                let dest_dir = self.objects_dir.join(&a_name).join(&b_name);
                for entry in std::fs::read_dir(&b)?.filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.extension().and_then(|e| e.to_str()) != Some("gz") {
                        continue;
                    }
                    let Some(fname) = p.file_name().map(|s| s.to_owned()) else {
                        continue;
                    };
                    std::fs::create_dir_all(&dest_dir)?;
                    let dest = dest_dir.join(&fname);
                    if dest.exists() {
                        let _ = std::fs::remove_file(&p); // already deduped here
                        continue;
                    }
                    if std::fs::rename(&p, &dest).is_err() {
                        // Cross-device (separate filesystem): copy then drop the original.
                        std::fs::copy(&p, &dest)?;
                        let _ = std::fs::remove_file(&p);
                    }
                    moved += 1;
                }
            }
        }
        Ok(moved)
    }

    fn path_for(&self, hash: &str) -> PathBuf {
        let (a, rest) = hash.split_at(2.min(hash.len()));
        let b = rest.get(0..2).unwrap_or("");
        self.objects_dir.join(a).join(b).join(format!("{hash}.gz"))
    }

    /// Store `bytes`, returning their blake3 hash. Idempotent: if the object already
    /// exists the write is skipped (content-addressed dedup). Blocking.
    pub fn put_blocking(&self, bytes: &[u8]) -> std::io::Result<String> {
        let hash = hashutil::hash_bytes(bytes);
        let path = self.path_for(&hash);
        if path.exists() {
            return Ok(hash); // dedup hit
        }
        let dir = path.parent().unwrap_or(&self.objects_dir);
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(format!("{hash}.tmp.{}", std::process::id()));
        {
            let f = std::fs::File::create(&tmp)?;
            let mut enc = GzEncoder::new(f, Compression::default());
            enc.write_all(bytes)?;
            enc.finish()?.sync_all()?;
        }
        // Atomic publish. If we lost a race to another writer of the *same* hash, the
        // bytes are identical, so accept theirs and drop our temp.
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            if !path.exists() {
                return Err(e);
            }
        }
        Ok(hash)
    }

    /// Read and decompress a stored object by hash. Blocking.
    pub fn get_blocking(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        let f = std::fs::File::open(self.path_for(hash))?;
        let mut dec = GzDecoder::new(f);
        let mut out = Vec::new();
        dec.read_to_end(&mut out)?;
        Ok(out)
    }

    pub fn exists(&self, hash: &str) -> bool {
        self.path_for(hash).exists()
    }

    /// Async wrapper: blob writes must never block an async worker.
    pub async fn put(&self, bytes: Vec<u8>) -> std::io::Result<String> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.put_blocking(&bytes))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Async wrapper around `get_blocking`.
    pub async fn get(&self, hash: &str) -> std::io::Result<Vec<u8>> {
        let store = self.clone();
        let hash = hash.to_string();
        tokio::task::spawn_blocking(move || store.get_blocking(&hash))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Mark/sweep: delete every object whose hash is not in `keep`. Returns
    /// (objects_removed, bytes_freed). In-flight `.tmp.` files are left alone, and
    /// emptied fanout directories are pruned. Blocking; wrap in spawn_blocking.
    pub fn gc_blocking(&self, keep: &HashSet<String>) -> std::io::Result<(usize, u64)> {
        let mut removed = 0usize;
        let mut freed = 0u64;
        if !self.objects_dir.exists() {
            return Ok((0, 0));
        }
        for a in read_subdirs(&self.objects_dir)? {
            for b in read_subdirs(&a)? {
                for entry in std::fs::read_dir(&b)?.filter_map(|e| e.ok()) {
                    let p = entry.path();
                    let stem = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if stem.contains(".tmp.") {
                        continue;
                    }
                    let hash = stem.strip_suffix(".gz").unwrap_or(stem);
                    if !keep.contains(hash) {
                        let len = entry.metadata().map(|m| m.len()).unwrap_or(0);
                        if std::fs::remove_file(&p).is_ok() {
                            removed += 1;
                            freed += len;
                        }
                    }
                }
                let _ = remove_dir_if_empty(&b);
            }
            let _ = remove_dir_if_empty(&a);
        }
        Ok((removed, freed))
    }
}

/// Shared object store dir under $XDG_STATE_HOME/notion-sync/ (mirrors state.db's
/// location), falling back to ~/.local/state when XDG_STATE_HOME is unset.
fn default_objects_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."));
            home.join(".local/state")
        });
    base.join("notion-sync").join("objects")
}

fn read_subdirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    Ok(std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect())
}

fn remove_dir_if_empty(dir: &Path) -> std::io::Result<()> {
    if std::fs::read_dir(dir)?.next().is_none() {
        std::fs::remove_dir(dir)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_roundtrip_and_dedup() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path());
        let data = b"hello \xF0\x9F\x9A\x80 world\n".to_vec();
        let h1 = store.put_blocking(&data).unwrap();
        // Same content -> same hash, second put is a no-op dedup hit.
        let h2 = store.put_blocking(&data).unwrap();
        assert_eq!(h1, h2);
        assert!(store.exists(&h1));
        assert_eq!(store.get_blocking(&h1).unwrap(), data);
    }

    #[test]
    fn gc_keeps_referenced_drops_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ObjectStore::new(tmp.path());
        let keep_hash = store.put_blocking(b"keep me").unwrap();
        let drop_hash = store.put_blocking(b"drop me").unwrap();
        let mut keep = HashSet::new();
        keep.insert(keep_hash.clone());
        let (removed, _freed) = store.gc_blocking(&keep).unwrap();
        assert_eq!(removed, 1);
        assert!(store.exists(&keep_hash));
        assert!(!store.exists(&drop_hash));
    }

    #[test]
    fn import_legacy_root_store_moves_blobs_into_shared_store() {
        // Old per-root store with one blob.
        let old_root = tempfile::tempdir().unwrap();
        let legacy = ObjectStore::new(old_root.path());
        let data = b"legacy snapshot bytes".to_vec();
        let hash = legacy.put_blocking(&data).unwrap();
        assert!(legacy.exists(&hash));

        // Fresh shared store in a separate location.
        let shared_dir = tempfile::tempdir().unwrap();
        let shared = ObjectStore::at(shared_dir.path().join("objects"));
        assert!(!shared.exists(&hash));

        let moved = shared.import_legacy_root_store(old_root.path()).unwrap();
        assert_eq!(moved, 1);
        assert!(shared.exists(&hash));
        assert_eq!(shared.get_blocking(&hash).unwrap(), data);
        // The blob was moved, not copied: the old store no longer has it.
        assert!(!legacy.exists(&hash));
    }
}

//! Per-path locking so the watcher (push) and poller (pull) never operate on the
//! same node concurrently.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Mutex;

#[derive(Clone, Default)]
pub struct PathLocks {
    map: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl PathLocks {
    pub fn new() -> Self {
        PathLocks {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get-or-create the lock for `rel_path`; the caller holds it for the critical section.
    pub async fn lock_for(&self, rel_path: &str) -> Arc<Mutex<()>> {
        let mut map = self.map.lock().await;
        map.entry(rel_path.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }
}

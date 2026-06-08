//! Sync subsystem: engine (orchestrator), watcher (local->Notion), poller
//! (Notion->local), conflict resolver, reconciliation, and shared helpers.

pub mod conflict;
pub mod engine;
pub mod locks;
pub mod poller;
pub mod reconcile;
pub mod snapshot;
pub mod util;
pub mod watcher;

pub use engine::Engine;

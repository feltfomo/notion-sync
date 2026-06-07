//! Two-way sync daemon mirroring a local codebase into a Notion page tree.
//! Local filesystem is the source of truth (local-wins).

pub mod api;
pub mod chunk;
pub mod config;
pub mod hashutil;
pub mod language;
pub mod state;
pub mod sync;

//! Notion API layer: rate-limited, retrying client + serde models.

pub mod client;
pub mod errors;
pub mod models;
pub mod ratelimit;

pub use client::NotionClient;
pub use errors::{ApiError, NotionApiError};
pub use ratelimit::RateLimiter;

//! Error type that always carries enough to log status + Notion code + message.

use std::fmt;

/// A failed Notion request: the HTTP status, Notion's `code` string, and the
/// message. We keep all three so a log line actually tells you what broke.
#[derive(Debug, Clone)]
pub struct NotionApiError {
    pub status: u16,
    /// Notion's machine-readable error code, e.g. "object_not_found", "rate_limited".
    pub code: String,
    pub message: String,
}

impl fmt::Display for NotionApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "notion api error: status={} code={} message={}",
            self.status, self.code, self.message
        )
    }
}

impl std::error::Error for NotionApiError {}

#[derive(Debug)]
pub enum ApiError {
    /// A non-retryable API error (4xx other than 409/429) with full context.
    Api(NotionApiError),
    /// 401 Unauthorized that survived a one-shot token reload: the token is revoked or
    /// expired and the daemon should stop rather than 401 every request forever.
    Unauthorized(NotionApiError),
    /// Transport/IO failure from reqwest.
    Transport(String),
    /// Body could not be (de)serialized.
    Serde(String),
    /// Retries exhausted; carries the last underlying error description.
    RetriesExhausted(String),
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Api(e) => write!(f, "{e}"),
            ApiError::Unauthorized(e) => write!(f, "unauthorized: {e}"),
            ApiError::Transport(e) => write!(f, "transport error: {e}"),
            ApiError::Serde(e) => write!(f, "serialization error: {e}"),
            ApiError::RetriesExhausted(e) => write!(f, "retries exhausted: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl From<reqwest::Error> for ApiError {
    fn from(e: reqwest::Error) -> Self {
        ApiError::Transport(e.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::Serde(e.to_string())
    }
}

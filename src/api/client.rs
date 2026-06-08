//! The single Notion HTTP client. All Notion traffic funnels through here so that
//! auth, the pinned version header, the shared rate limiter, and retry/backoff are
//! applied uniformly.

use std::sync::Arc;
use std::time::Duration;

use reqwest::{Method, Response, StatusCode};
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use super::errors::{ApiError, NotionApiError};
use super::models::*;
use super::ratelimit::RateLimiter;

const API_BASE: &str = "https://api.notion.com";
const MAX_RETRIES: u32 = 6;

pub struct NotionClient {
    http: reqwest::Client,
    token: String,
    version: String,
    limiter: Arc<RateLimiter>,
}

impl NotionClient {
    pub fn new(
        token: String,
        version: String,
        limiter: Arc<RateLimiter>,
    ) -> Result<Self, ApiError> {
        let http = reqwest::Client::builder()
            .user_agent("notion-sync/0.1")
            .build()?;
        Ok(NotionClient {
            http,
            token,
            version,
            limiter,
        })
    }

    /// Core request primitive: rate-limit, send, classify status, retry transient.
    async fn request_json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<T, ApiError> {
        let url = format!("{API_BASE}{path}");
        let mut attempt: u32 = 0;

        loop {
            // Draw from the shared bucket before every send (including retries).
            self.limiter.acquire().await;

            let mut req = self
                .http
                .request(method.clone(), &url)
                .bearer_auth(&self.token)
                .header("Notion-Version", &self.version);
            if let Some(b) = body {
                req = req.json(b);
            }

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    // Network-level error: treat as transient.
                    if attempt >= MAX_RETRIES {
                        return Err(ApiError::RetriesExhausted(e.to_string()));
                    }
                    let backoff = backoff_with_jitter(attempt);
                    warn!(error = %e, attempt, backoff_ms = backoff.as_millis() as u64, "transport error, retrying");
                    tokio::time::sleep(backoff).await;
                    attempt += 1;
                    continue;
                }
            };

            let status = resp.status();
            if status.is_success() {
                let bytes = resp.bytes().await?;
                return Ok(serde_json::from_slice::<T>(&bytes)?);
            }

            // Non-2xx. Decide retry vs surface.
            let retry_after = parse_retry_after(&resp);
            let (code, message) = read_error_body(resp).await;

            match status {
                StatusCode::TOO_MANY_REQUESTS => {
                    if attempt >= MAX_RETRIES {
                        return Err(ApiError::Api(NotionApiError { status: status.as_u16(), code, message }));
                    }
                    let wait = retry_after.unwrap_or_else(|| backoff_with_jitter(attempt));
                    warn!(status = 429, code = %code, wait_ms = wait.as_millis() as u64, "rate limited, honoring Retry-After");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                StatusCode::CONFLICT // 409 conflict_error (concurrent edit)
                | StatusCode::BAD_GATEWAY
                | StatusCode::SERVICE_UNAVAILABLE
                | StatusCode::GATEWAY_TIMEOUT => {
                    if attempt >= MAX_RETRIES {
                        return Err(ApiError::Api(NotionApiError { status: status.as_u16(), code, message }));
                    }
                    let wait = backoff_with_jitter(attempt);
                    warn!(status = status.as_u16(), code = %code, wait_ms = wait.as_millis() as u64, "transient error, backing off");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                // 529 (overloaded) is non-standard; reqwest exposes it as raw u16.
                s if s.as_u16() == 529 => {
                    if attempt >= MAX_RETRIES {
                        return Err(ApiError::Api(NotionApiError { status: 529, code, message }));
                    }
                    let wait = backoff_with_jitter(attempt);
                    warn!(status = 529, code = %code, wait_ms = wait.as_millis() as u64, "overloaded, backing off");
                    tokio::time::sleep(wait).await;
                    attempt += 1;
                }
                _ => {
                    // Permanent (4xx other than 409/429, or unexpected). Surface with full context.
                    return Err(ApiError::Api(NotionApiError { status: status.as_u16(), code, message }));
                }
            }
        }
    }

    /// GET /v1/users/me — learn our own bot user id (echo-loop suppression).
    pub async fn whoami(&self) -> Result<String, ApiError> {
        let me: MeResp = self.request_json(Method::GET, "/v1/users/me", None).await?;
        Ok(me.id)
    }

    /// POST /v1/pages — create a subpage under `parent_page_id` with `title`.
    /// Optional initial `children` blocks (e.g. binary placeholder callout).
    pub async fn create_page(
        &self,
        parent_page_id: &str,
        title: &str,
        children: Vec<serde_json::Value>,
    ) -> Result<PageResp, ApiError> {
        let body = serde_json::json!({
            "parent": { "type": "page_id", "page_id": parent_page_id },
            "properties": title_properties(title),
            "children": children,
        });
        self.request_json(Method::POST, "/v1/pages", Some(&body))
            .await
    }

    pub async fn get_page(&self, page_id: &str) -> Result<PageResp, ApiError> {
        let path = format!("/v1/pages/{page_id}");
        self.request_json(Method::GET, &path, None).await
    }

    /// PATCH /v1/pages/{id} — rename (title) and/or reparent.
    pub async fn update_page(
        &self,
        page_id: &str,
        new_title: Option<&str>,
        new_parent_page_id: Option<&str>,
        in_trash: Option<bool>,
    ) -> Result<PageResp, ApiError> {
        let mut body = serde_json::Map::new();
        if let Some(t) = new_title {
            body.insert("properties".into(), title_properties(t));
        }
        if let Some(p) = new_parent_page_id {
            body.insert(
                "parent".into(),
                serde_json::json!({ "type": "page_id", "page_id": p }),
            );
        }
        if let Some(trash) = in_trash {
            // PATCH /v1/pages uses `archived` to move a page to / out of trash.
            body.insert("archived".into(), serde_json::json!(trash));
        }
        let path = format!("/v1/pages/{page_id}");
        self.request_json(Method::PATCH, &path, Some(&serde_json::Value::Object(body)))
            .await
    }

    /// PATCH /v1/blocks/{id}/children — append up to 100 blocks per call.
    pub async fn append_children(
        &self,
        block_id: &str,
        children: Vec<serde_json::Value>,
    ) -> Result<Vec<String>, ApiError> {
        debug_assert!(children.len() <= 100, "caller must batch to <=100 children");
        let body = serde_json::json!({ "children": children });
        let path = format!("/v1/blocks/{block_id}/children");
        let resp: ChildrenListResp = self.request_json(Method::PATCH, &path, Some(&body)).await?;
        Ok(resp.results.into_iter().map(|b| b.id).collect())
    }

    /// GET /v1/blocks/{id}/children — fully paginated.
    pub async fn list_children(&self, block_id: &str) -> Result<Vec<BlockResp>, ApiError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let path = match &cursor {
                Some(c) => format!("/v1/blocks/{block_id}/children?page_size=100&start_cursor={c}"),
                None => format!("/v1/blocks/{block_id}/children?page_size=100"),
            };
            let resp: ChildrenListResp = self.request_json(Method::GET, &path, None).await?;
            out.extend(resp.results);
            if resp.has_more {
                cursor = resp.next_cursor;
                if cursor.is_none() {
                    // Notion promised more pages but gave us no cursor. Surface it:
                    // silently breaking here would drop children and could cause a
                    // destructive partial readback elsewhere.
                    warn!(
                        block_id,
                        fetched = out.len(),
                        "list_children: has_more=true but next_cursor=null; listing truncated"
                    );
                    break;
                }
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// DELETE /v1/blocks/{id} — moves the block to trash (overwrite strategy).
    pub async fn delete_block(&self, block_id: &str) -> Result<(), ApiError> {
        let path = format!("/v1/blocks/{block_id}");
        let _: serde_json::Value = self.request_json(Method::DELETE, &path, None).await?;
        Ok(())
    }
}

/// Exponential backoff with full jitter: base 500ms, doubling, capped at 30s.
fn backoff_with_jitter(attempt: u32) -> Duration {
    let base_ms: u64 = 500;
    let exp = base_ms.saturating_mul(1u64 << attempt.min(6));
    let capped = exp.min(30_000);
    // Cheap, dependency-free jitter from the system clock nanos.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let jitter = nanos % (capped + 1);
    Duration::from_millis(jitter)
}

/// Parse `Retry-After`. Notion only ever sends an integer number of seconds, so the
/// RFC 7231 HTTP-date form is deliberately not handled; if it ever appears we return
/// `None` and the caller falls back to jittered exponential backoff.
fn parse_retry_after(resp: &Response) -> Option<Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Extract Notion's `code` and `message` from an error body, with safe fallbacks.
async fn read_error_body(resp: Response) -> (String, String) {
    let status = resp.status();
    match resp.text().await {
        Ok(text) => {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                let code = v
                    .get("code")
                    .and_then(|c| c.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let message = v
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or(&text)
                    .to_string();
                debug!(status = status.as_u16(), code = %code, "notion error body parsed");
                (code, message)
            } else {
                ("unknown".to_string(), text)
            }
        }
        Err(_) => ("unknown".to_string(), format!("http {}", status.as_u16())),
    }
}

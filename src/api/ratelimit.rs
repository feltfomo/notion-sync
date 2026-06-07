//! A single shared token-bucket rate limiter.
//!
//! There's exactly one of these per process, shared (via Arc) between the watcher's
//! push path and the poller's pull path. They are NOT separate clients. Notion's
//! ~3 req/s limit is per-integration, so both directions have to draw from the same
//! bucket or we trip 429s under load.

use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

pub struct RateLimiter {
    inner: Mutex<Bucket>,
    capacity: f64,
    refill_per_sec: f64,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// `rate` = sustained requests/sec (~3.0 for Notion). `burst` = bucket capacity.
    pub fn new(rate: f64, burst: f64) -> Self {
        RateLimiter {
            inner: Mutex::new(Bucket {
                tokens: burst,
                last_refill: Instant::now(),
            }),
            capacity: burst,
            refill_per_sec: rate,
        }
    }

    /// Notion default: ~3 req/s sustained, allow a small burst.
    pub fn notion_default() -> Self {
        RateLimiter::new(3.0, 3.0)
    }

    /// Block until a token is available, then consume one. Async; never busy-waits.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut b = self.inner.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(b.last_refill).as_secs_f64();
                b.tokens = (b.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                b.last_refill = now;

                if b.tokens >= 1.0 {
                    b.tokens -= 1.0;
                    return;
                }
                // Time until the next whole token is available.
                let deficit = 1.0 - b.tokens;
                Duration::from_secs_f64(deficit / self.refill_per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

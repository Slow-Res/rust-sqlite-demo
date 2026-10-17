//! Per-process token buckets (spec §12.3). The effective global rate is
//! `per_second × processes`; that is documented, not hidden.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets keyed by a handler-chosen string, e.g. `webhook:example.com`.
/// Capacity (burst) is `max(1, per_second)`.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait until one call for `key` fits `per_second`. Non-positive rates do not limit.
    pub async fn acquire(&self, key: &str, per_second: f64) {
        if per_second <= 0.0 || !per_second.is_finite() {
            return;
        }
        let capacity = per_second.max(1.0);
        loop {
            let wait = {
                let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
                    tokens: capacity,
                    last: now,
                });
                let refill = now.duration_since(bucket.last).as_secs_f64() * per_second;
                bucket.tokens = (bucket.tokens + refill).min(capacity);
                bucket.last = now;
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }
                Duration::from_secs_f64((1.0 - bucket.tokens) / per_second)
            };
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn bursts_then_paces_per_key() {
        let limiter = RateLimiter::new();
        let start = tokio::time::Instant::now();
        for _ in 0..10 {
            limiter.acquire("host-a", 5.0).await;
        }
        // 5 tokens of burst, then one every 200 ms.
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(990) && elapsed <= Duration::from_millis(1010),
            "{elapsed:?}"
        );
        // Another key has its own bucket.
        let start = tokio::time::Instant::now();
        limiter.acquire("host-b", 5.0).await;
        assert!(start.elapsed() < Duration::from_millis(1));
    }
}

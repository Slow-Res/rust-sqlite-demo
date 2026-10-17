//! Per-process token buckets (spec §12.3). The effective global rate is
//! `per_second × processes`; that is documented, not hidden.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// Above this many keys, `acquire` first drops the buckets that are full again.
/// A full bucket is indistinguishable from a fresh one, so dropping it loses
/// nothing; only keys that are still being paced stay, which bounds the map.
const MAX_IDLE_KEYS: usize = 1024;

/// No single wait is longer than this. A rate below one call per hour is
/// therefore enforced as one call per hour (and it keeps `Duration` finite).
const MAX_WAIT: Duration = Duration::from_secs(3600);

struct Bucket {
    tokens: f64,
    last: Instant,
    /// The rate this bucket was last used with, to tell when it has refilled.
    per_second: f64,
}

impl Bucket {
    fn is_full_at(&self, now: Instant) -> bool {
        let refill = now.duration_since(self.last).as_secs_f64() * self.per_second;
        self.tokens + refill >= self.per_second.max(1.0)
    }
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
            let (wait, capped) = {
                let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                if buckets.len() > MAX_IDLE_KEYS {
                    buckets.retain(|_, b| !b.is_full_at(now));
                }
                let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
                    tokens: capacity,
                    last: now,
                    per_second,
                });
                bucket.per_second = per_second;
                let refill = now.duration_since(bucket.last).as_secs_f64() * per_second;
                bucket.tokens = (bucket.tokens + refill).min(capacity);
                bucket.last = now;
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }
                let secs = (1.0 - bucket.tokens) / per_second;
                match Duration::try_from_secs_f64(secs) {
                    Ok(wait) if wait <= MAX_WAIT => (wait, false),
                    _ => (MAX_WAIT, true),
                }
            };
            tokio::time::sleep(wait).await;
            if capped {
                return;
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.buckets
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
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

    #[tokio::test(start_paused = true)]
    async fn full_buckets_are_evicted_and_live_keys_keep_pacing() {
        let limiter = RateLimiter::new();
        for _ in 0..5 {
            limiter.acquire("live", 5.0).await;
        }
        for i in 0..2000 {
            limiter.acquire(&format!("k{i}"), 5.0).await;
        }
        assert_eq!(
            limiter.len(),
            2001,
            "nothing is full yet, so nothing is dropped"
        );
        // The drained key was not evicted: it still has to wait for a token.
        let start = tokio::time::Instant::now();
        limiter.acquire("live", 5.0).await;
        let waited = start.elapsed();
        assert!(
            waited >= Duration::from_millis(190) && waited <= Duration::from_millis(210),
            "{waited:?}"
        );
        // After a refill every idle bucket is full again and is dropped.
        tokio::time::advance(Duration::from_secs(2)).await;
        limiter.acquire("fresh", 5.0).await;
        assert!(
            limiter.len() <= MAX_IDLE_KEYS,
            "{} buckets kept",
            limiter.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tiny_rate_waits_at_most_the_cap_and_does_not_panic() {
        let limiter = RateLimiter::new();
        limiter.acquire("k", 1e-20).await;
        let start = tokio::time::Instant::now();
        limiter.acquire("k", 1e-20).await;
        assert!(
            start.elapsed() <= MAX_WAIT + Duration::from_millis(1),
            "{:?}",
            start.elapsed()
        );
    }
}

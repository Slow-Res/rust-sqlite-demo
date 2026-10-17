//! Per-process token buckets (spec §12.3). The effective global rate is
//! `per_second × processes`; that is documented, not hidden.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

/// A sweep for full (idle) buckets runs when the map outgrows this many keys.
/// A full bucket is indistinguishable from a fresh one, so dropping it loses
/// nothing; only keys that are still being paced stay, which bounds the map.
/// The threshold then doubles past what survived, so a map full of busy keys is
/// not rescanned on every call.
const MAX_IDLE_KEYS: usize = 1024;

/// The slowest pace enforced: one call per hour. Slower rates are clamped to it,
/// which keeps every wait finite and at most [`MAX_WAIT`].
const MIN_RATE: f64 = 1.0 / 3600.0;

/// Longest single sleep; only a fallback, since the clamp keeps waits below it.
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

struct State {
    buckets: HashMap<String, Bucket>,
    /// Sweep when `buckets.len()` exceeds this.
    next_sweep: usize,
    #[cfg(test)]
    sweeps: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            buckets: HashMap::new(),
            next_sweep: MAX_IDLE_KEYS,
            #[cfg(test)]
            sweeps: 0,
        }
    }
}

impl State {
    fn sweep_if_needed(&mut self, now: Instant) {
        if self.buckets.len() <= self.next_sweep {
            return;
        }
        self.buckets.retain(|_, b| !b.is_full_at(now));
        self.next_sweep = MAX_IDLE_KEYS.max(2 * self.buckets.len());
        #[cfg(test)]
        {
            self.sweeps += 1;
        }
    }
}

/// Token buckets keyed by a handler-chosen string, e.g. `webhook:example.com`.
/// Capacity (burst) is `max(1, per_second)`. Rates below one call per hour are
/// enforced as one call per hour.
#[derive(Default)]
pub struct RateLimiter {
    state: Mutex<State>,
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
        let per_second = per_second.max(MIN_RATE);
        let capacity = per_second.max(1.0);
        loop {
            let wait = {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                state.sweep_if_needed(now);
                let bucket = state.buckets.entry(key.to_owned()).or_insert(Bucket {
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
                Duration::try_from_secs_f64(secs)
                    .unwrap_or(MAX_WAIT)
                    .min(MAX_WAIT)
            };
            // Always re-check: a concurrent caller may have taken the token.
            tokio::time::sleep(wait).await;
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .buckets
            .len()
    }

    #[cfg(test)]
    fn sweeps(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .sweeps
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
        // The threshold was doubled to 2050, so it takes new keys to trigger the next sweep.
        for i in 0..100 {
            limiter.acquire(&format!("fresh{i}"), 5.0).await;
        }
        assert_eq!(limiter.sweeps(), 2);
        assert!(
            limiter.len() <= MAX_IDLE_KEYS,
            "{} buckets kept",
            limiter.len()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn tiny_rate_is_clamped_to_one_per_hour_and_never_bursts() {
        let limiter = RateLimiter::new();
        limiter.acquire("k", 1e-20).await;
        let start = tokio::time::Instant::now();
        // Two waiters at once: each needs its own token, so they pass an hour apart.
        let pass = || async {
            limiter.acquire("k", 1e-20).await;
            start.elapsed()
        };
        let (a, b) = tokio::join!(pass(), pass());
        let (first, second) = (a.min(b), a.max(b));
        assert!(first >= Duration::from_secs(3599), "{first:?}");
        assert!(
            second - first >= Duration::from_secs(3599),
            "{first:?} {second:?}"
        );
    }
}

//! Retry delays: exponential with full jitter (spec §9.2). Jitter spreads the
//! retries of jobs that failed together (an outage) instead of stampeding.

use std::time::Duration;

/// `delay(n)` is uniform in `[0, min(cap, base × 2^(n-1))]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(5),
            cap: Duration::from_secs(3600),
        }
    }
}

impl Backoff {
    /// The largest delay after the `attempt`-th failure (1-based; 0 counts as 1).
    pub fn ceiling(&self, attempt: i32) -> Duration {
        let doublings = u32::try_from(attempt.saturating_sub(1).max(0))
            .unwrap_or(0)
            .min(30);
        self.base.saturating_mul(1u32 << doublings).min(self.cap)
    }

    /// A random delay in `[0, ceiling(attempt)]` (full jitter).
    pub fn delay(&self, attempt: i32) -> Duration {
        let max = self.ceiling(attempt).as_secs_f64();
        if max <= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(rand::random_range(0.0..=max))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_doubles_from_base_and_stops_at_cap() {
        let b = Backoff::default();
        assert_eq!(b.ceiling(0), Duration::from_secs(5));
        assert_eq!(b.ceiling(1), Duration::from_secs(5));
        assert_eq!(b.ceiling(2), Duration::from_secs(10));
        assert_eq!(b.ceiling(3), Duration::from_secs(20));
        assert_eq!(b.ceiling(11), Duration::from_secs(3600));
        assert_eq!(b.ceiling(i32::MAX), Duration::from_secs(3600));
    }

    #[test]
    fn delay_stays_within_zero_and_ceiling() {
        let b = Backoff {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(8),
        };
        for attempt in 1..=6 {
            for _ in 0..200 {
                let d = b.delay(attempt);
                assert!(
                    d <= b.ceiling(attempt),
                    "{d:?} > ceiling for attempt {attempt}"
                );
            }
        }
    }
}

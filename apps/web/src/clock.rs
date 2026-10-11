//! Time source abstraction so business rules (due dates, overdue) are testable.

use std::sync::Mutex;

use chrono::{DateTime, Duration, Utc};

/// Anything that can tell the current time. Services hold `Arc<dyn Clock>`
/// (a shared, dynamically-dispatched handle) so tests can swap in [`FixedClock`].
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// The real wall clock, truncated to whole seconds. The database stores Unix
/// seconds, so handing services a sub-second `now` would let in-memory
/// comparisons (`due_at < now`) disagree with SQL filters bound to the stored,
/// truncated value. Truncating at the source keeps every caller consistent.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        truncate_to_seconds(Utc::now())
    }
}

fn truncate_to_seconds(at: DateTime<Utc>) -> DateTime<Utc> {
    // Seconds within `at`'s own range are always representable, so the
    // fallback is unreachable; it only avoids an `unwrap`.
    DateTime::from_timestamp(at.timestamp(), 0).unwrap_or(at)
}

/// A clock you control. Intended for tests: set or advance time and every
/// holder of the same `Arc<FixedClock>` sees it. Interior mutability (the
/// `Mutex`) lets `set`/`advance` take `&self`, which is required because the
/// clock is shared behind an `Arc`.
#[derive(Debug)]
pub struct FixedClock(Mutex<DateTime<Utc>>);

impl FixedClock {
    pub fn new(now: DateTime<Utc>) -> Self {
        Self(Mutex::new(now))
    }

    pub fn set(&self, now: DateTime<Utc>) {
        *self.lock() = now;
    }

    pub fn advance(&self, by: Duration) {
        *self.lock() += by;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DateTime<Utc>> {
        // A poisoned lock only means another test panicked; the timestamp is still valid.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        *self.lock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_clock_has_no_sub_second_part() {
        assert_eq!(SystemClock.now().timestamp_subsec_nanos(), 0);
        let at = DateTime::from_timestamp(1_800_000_000, 999_999_999).unwrap();
        assert_eq!(
            truncate_to_seconds(at),
            DateTime::from_timestamp(1_800_000_000, 0).unwrap()
        );
    }

    #[test]
    fn fixed_clock_set_and_advance() {
        let start = DateTime::from_timestamp(1_800_000_000, 0).unwrap();
        let clock = FixedClock::new(start);
        assert_eq!(clock.now(), start);
        clock.advance(Duration::days(14));
        assert_eq!(clock.now(), start + Duration::days(14));
        clock.set(start);
        assert_eq!(clock.now(), start);
    }
}

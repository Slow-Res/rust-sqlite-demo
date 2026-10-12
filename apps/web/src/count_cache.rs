//! A short-lived cache for the `total` of the unfiltered list pages (and of the
//! book list filtered by availability alone).
//!
//! Every list request counts the matching rows, and counting the whole table
//! (50k books, 10k members, 20k+ loans) walks an entire index on every request.
//! The total of an *unfiltered* list is the same for every caller, so it is
//! kept for `LIST_COUNT_TTL_MS` (default 1000) and shared. Only totals are
//! cached: every page of rows is still read from the database, and search
//! counts are never cached (the index bounds them). The availability totals of
//! the book list are cached because they are a scan of the active loans, as
//! many on every request as there are loans. With a TTL of 0 nothing is kept
//! and every request counts.
//!
//! The price is that `total` may lag behind a write by up to the TTL.

use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

/// The lists whose unfiltered total is cached. Few and fixed, so each has its
/// own slot instead of a map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountKey {
    Books,
    BooksAvailable,
    BooksOnLoan,
    Members,
    LoansActive,
    LoansReturned,
    LoansAll,
}

const KEYS: usize = 7;

impl CountKey {
    fn slot(self) -> usize {
        match self {
            Self::Books => 0,
            Self::Members => 1,
            Self::LoansActive => 2,
            Self::LoansReturned => 3,
            Self::LoansAll => 4,
            Self::BooksAvailable => 5,
            Self::BooksOnLoan => 6,
        }
    }
}

#[derive(Debug)]
pub struct CountCache {
    ttl: Duration,
    /// One slot per key: when the count was taken, and what it was. The lock
    /// is held while counting, so when a slot expires under load one request
    /// counts and the others wait for its answer instead of all counting.
    slots: [Mutex<Option<(Instant, u64)>>; KEYS],
}

impl CountCache {
    /// `ttl` of zero disables the cache.
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slots: std::array::from_fn(|_| Mutex::new(None)),
        }
    }

    /// [`get_or_count`](Self::get_or_count) for a list that may not be cacheable
    /// (`key` is `None` for a filtered list): then it just counts.
    pub async fn get_or_count_opt<E, F>(
        &self,
        key: Option<CountKey>,
        count: impl FnOnce() -> F,
    ) -> Result<u64, E>
    where
        F: Future<Output = Result<u64, E>>,
    {
        match key {
            Some(key) => self.get_or_count(key, count).await,
            None => count().await,
        }
    }

    /// The cached total for `key` if it is younger than the TTL, otherwise
    /// the result of `count`, which is then cached. A failed count is not cached.
    pub async fn get_or_count<E, F>(
        &self,
        key: CountKey,
        count: impl FnOnce() -> F,
    ) -> Result<u64, E>
    where
        F: Future<Output = Result<u64, E>>,
    {
        self.get_or_count_at(Instant::now(), key, count).await
    }

    /// [`get_or_count`](Self::get_or_count) with the current time passed in, so
    /// tests do not have to sleep.
    async fn get_or_count_at<E, F>(
        &self,
        now: Instant,
        key: CountKey,
        count: impl FnOnce() -> F,
    ) -> Result<u64, E>
    where
        F: Future<Output = Result<u64, E>>,
    {
        if self.ttl.is_zero() {
            return count().await;
        }
        let mut slot = self.slots[key.slot()].lock().await;
        if let Some((taken_at, total)) = *slot
            && now.saturating_duration_since(taken_at) < self.ttl
        {
            return Ok(total);
        }
        let total = count().await?;
        // Stamped when the count was requested: a count that ran long must not
        // look fresher than it is.
        *slot = Some((now, total));
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::convert::Infallible;

    use super::*;

    /// A count that answers `value` and notes how often it ran.
    async fn counted(calls: &Cell<u32>, value: u64) -> Result<u64, Infallible> {
        calls.set(calls.get() + 1);
        Ok(value)
    }

    #[tokio::test]
    async fn a_hit_within_the_ttl_does_not_count_again() {
        let cache = CountCache::new(Duration::from_millis(1000));
        let calls = Cell::new(0);
        let t0 = Instant::now();

        let first = cache
            .get_or_count_at(t0, CountKey::Books, || counted(&calls, 7))
            .await;
        let second = cache
            .get_or_count_at(t0 + Duration::from_millis(999), CountKey::Books, || {
                counted(&calls, 8)
            })
            .await;

        assert_eq!(first, Ok(7));
        assert_eq!(second, Ok(7), "the cached total, not a new count");
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn a_miss_after_the_ttl_counts_again() {
        let cache = CountCache::new(Duration::from_millis(1000));
        let calls = Cell::new(0);
        let t0 = Instant::now();

        cache
            .get_or_count_at(t0, CountKey::Books, || counted(&calls, 7))
            .await
            .unwrap();
        let later = cache
            .get_or_count_at(t0 + Duration::from_millis(1000), CountKey::Books, || {
                counted(&calls, 8)
            })
            .await;

        assert_eq!(later, Ok(8));
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn a_ttl_of_zero_always_counts() {
        let cache = CountCache::new(Duration::ZERO);
        let calls = Cell::new(0);
        let t0 = Instant::now();

        for expected in [1, 2, 3] {
            let total = cache
                .get_or_count_at(t0, CountKey::Members, || counted(&calls, expected))
                .await;
            assert_eq!(total, Ok(expected));
        }
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn keys_do_not_share_totals() {
        let cache = CountCache::new(Duration::from_millis(1000));
        let calls = Cell::new(0);
        let t0 = Instant::now();

        let active = cache
            .get_or_count_at(t0, CountKey::LoansActive, || counted(&calls, 1))
            .await;
        let returned = cache
            .get_or_count_at(t0, CountKey::LoansReturned, || counted(&calls, 2))
            .await;

        assert_eq!((active, returned), (Ok(1), Ok(2)));
        assert_eq!(calls.get(), 2);
    }

    #[tokio::test]
    async fn a_failed_count_is_not_cached() {
        let cache = CountCache::new(Duration::from_millis(1000));
        let t0 = Instant::now();

        let failed = cache
            .get_or_count_at(t0, CountKey::Books, || async { Err::<u64, _>("boom") })
            .await;
        let retried = cache
            .get_or_count_at(t0, CountKey::Books, || async { Ok::<_, &str>(5) })
            .await;

        assert_eq!(failed, Err("boom"));
        assert_eq!(retried, Ok(5));
    }
}

//! Background WAL checkpoints for SQLite (`SQLITE_CHECKPOINT_INTERVAL_MS`).
//!
//! SQLite's automatic checkpoint is PASSIVE: it copies what it can and never
//! waits for readers. Under constant read traffic it can never reset the WAL, so
//! the file only grows (2.5 GB for a 43 MB database in the load test) and every
//! read has more WAL to search. `PRAGMA wal_checkpoint(TRUNCATE)` waits (up to
//! the busy timeout) for the readers on the WAL to finish, copies everything,
//! and empties the file. Off by default: with Litestream, Litestream owns the
//! checkpoints (docs/ARCHITECTURE.md, S16).

use std::time::Duration;

use sqlx::SqlitePool;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

/// What `PRAGMA wal_checkpoint` reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointOutcome {
    /// A reader or writer kept it from finishing (the busy timeout ran out).
    pub busy: bool,
    /// Pages in the WAL when it stopped.
    pub wal_pages: i64,
    /// Pages copied into the database file.
    pub checkpointed_pages: i64,
}

impl CheckpointOutcome {
    /// Pages still waiting in the WAL.
    pub fn pending_pages(&self) -> i64 {
        self.wal_pages - self.checkpointed_pages
    }
}

/// Checkpoint the WAL and truncate it to zero bytes, on one connection of `pool`.
pub async fn checkpoint_truncate(pool: &SqlitePool) -> Result<CheckpointOutcome, sqlx::Error> {
    let (busy, wal_pages, checkpointed_pages): (i64, i64, i64) =
        sqlx::query_as("PRAGMA wal_checkpoint(TRUNCATE)")
            .fetch_one(pool)
            .await?;
    Ok(CheckpointOutcome {
        busy: busy != 0,
        wal_pages,
        checkpointed_pages,
    })
}

/// Run [`checkpoint_truncate`] every `every` until the returned task is aborted.
/// A failed or busy round is logged and the next one tries again.
pub fn spawn(pool: SqlitePool, every: Duration) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(every);
        // A slow round must not be followed by a burst of catch-up rounds.
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ticker.tick().await; // the first tick is immediate; wait a full period
        loop {
            ticker.tick().await;
            match checkpoint_truncate(&pool).await {
                Ok(outcome) if outcome.busy => tracing::warn!(
                    pending_pages = outcome.pending_pages(),
                    wal_pages = outcome.wal_pages,
                    "WAL checkpoint was busy; the WAL was not truncated"
                ),
                Ok(outcome) => tracing::debug!(
                    checkpointed_pages = outcome.checkpointed_pages,
                    "WAL checkpoint done"
                ),
                Err(err) => tracing::warn!(error = ?err, "WAL checkpoint failed"),
            }
        }
    })
}

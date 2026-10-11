//! The worker's main loop (spec §6.6–6.8): wait for the database and its
//! schema, then run Postgres sessions, retrying with growing waits, until
//! shutdown. A cycle in progress always finishes before shutting down.

use std::time::Duration;

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::changes::{self, Checkpoint};
use crate::config::Config;
use crate::metrics::{Metrics, lag_seconds, now_ms, now_secs};
use crate::mirror::{ConnectError, Mirror};
use crate::schema::sqlite_schema_ready;
use crate::sync::{Outcome, Syncer};

/// SQLite connections the worker uses.
const SQLITE_CONNECTIONS: u32 = 2;

/// Timing of the loop. Tests use much shorter values than production.
#[derive(Debug, Clone)]
pub struct Settings {
    pub batch_size: usize,
    /// Wait after a batch that had changes; also the shortest idle wait.
    pub poll_min: Duration,
    /// Longest idle wait.
    pub poll_max: Duration,
    /// First wait after a failed session; doubles up to `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Wait before trying again for a lock another worker holds.
    pub lock_retry: Duration,
    /// Wait between checks for the database file and its schema.
    pub wait_step: Duration,
}

impl Settings {
    /// The values from spec §6.3 and §6.8.
    pub fn production(batch_size: usize) -> Self {
        Self {
            batch_size,
            poll_min: Duration::from_millis(200),
            poll_max: Duration::from_secs(2),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            lock_retry: Duration::from_secs(30),
            wait_step: Duration::from_secs(1),
        }
    }
}

/// How long to wait before the next cycle (spec §6.3 step 4).
pub fn next_poll(outcome: &Outcome, previous: Duration, settings: &Settings) -> Duration {
    match outcome {
        Outcome::Shipped { full: true, .. } => Duration::ZERO,
        Outcome::Shipped { .. } | Outcome::Copied { .. } => settings.poll_min,
        Outcome::Idle => (previous * 2).clamp(settings.poll_min, settings.poll_max),
    }
}

/// Wait after a failed session: doubles from `retry_min` up to `retry_max`.
pub fn next_retry(previous: Duration, settings: &Settings) -> Duration {
    (previous * 2).clamp(settings.retry_min, settings.retry_max)
}

enum SessionError {
    Locked(String),
    Failed(anyhow::Error),
}

/// Run until `shutdown` is cancelled. Every failure is retried; nothing here
/// gives up, because a worker that exits would stop the backup silently.
pub async fn run(
    config: &Config,
    settings: &Settings,
    metrics: &Metrics,
    shutdown: &CancellationToken,
) -> anyhow::Result<()> {
    let Some(sqlite) = wait_for_sqlite(&config.database_url, settings, shutdown).await else {
        return Ok(());
    };
    let mut retry = Duration::ZERO;
    // Last checkpoint position this process saw; survives reconnects so the
    // backlog gauges stay right while sessions fail.
    let mut known_seq = 0;
    while !shutdown.is_cancelled() {
        if !wait_for_schema(&sqlite, settings, shutdown).await {
            break;
        }
        match session(
            &sqlite,
            config,
            settings,
            metrics,
            shutdown,
            &mut retry,
            &mut known_seq,
        )
        .await
        {
            Ok(()) => break,
            Err(SessionError::Locked(node)) => {
                metrics.errors.inc();
                refresh_backlog(metrics, &sqlite, known_seq).await;
                tracing::error!(
                    node,
                    "another sync-worker holds this node's lock; is NODE_ID used twice? retrying"
                );
                sleep_or_cancel(settings.lock_retry, shutdown).await;
            }
            Err(SessionError::Failed(err)) => {
                metrics.errors.inc();
                retry = next_retry(retry, settings);
                refresh_backlog(metrics, &sqlite, known_seq).await;
                tracing::warn!(error = format!("{err:#}"), retry_in = ?retry, "sync session failed; retrying");
                sleep_or_cancel(retry, shutdown).await;
            }
        }
    }
    sqlite.close().await;
    Ok(())
}

/// One Postgres session: connect, lock, then cycle until shutdown (Ok) or
/// an error (Err). `retry` is reset after every successful cycle.
async fn session(
    sqlite: &SqlitePool,
    config: &Config,
    settings: &Settings,
    metrics: &Metrics,
    shutdown: &CancellationToken,
    retry: &mut Duration,
    known_seq: &mut i64,
) -> Result<(), SessionError> {
    let mirror = match Mirror::connect(&config.postgres, &config.node_id).await {
        Ok(mirror) => mirror,
        Err(ConnectError::Locked(node)) => return Err(SessionError::Locked(node)),
        Err(ConnectError::Other(err)) => return Err(SessionError::Failed(err)),
    };
    tracing::info!(node = %config.node_id, "connected to Postgres and holding the node lock");
    let mut syncer = Syncer::new(sqlite.clone(), mirror, settings.batch_size);
    let mut wait = Duration::ZERO;
    loop {
        if shutdown.is_cancelled() {
            syncer.into_mirror().close().await;
            return Ok(());
        }
        let outcome = syncer.cycle().await.map_err(SessionError::Failed)?;
        record_outcome(metrics, &outcome, syncer.checkpoint());
        // A failed prune leaves shipped rows in the log that no gauge counts.
        metrics.errors.inc_by(syncer.take_prune_failures());
        if let Some(checkpoint) = syncer.checkpoint() {
            *known_seq = checkpoint.last_seq;
        }
        refresh_backlog(metrics, sqlite, *known_seq).await;
        metrics.last_success_seconds.set(now_secs());
        *retry = Duration::ZERO;
        wait = next_poll(&outcome, wait, settings);
        sleep_or_cancel(wait, shutdown).await;
    }
}

/// Count the cycle's work and note the epoch of this node's copy.
fn record_outcome(metrics: &Metrics, outcome: &Outcome, checkpoint: Option<&Checkpoint>) {
    match outcome {
        Outcome::Shipped { rows, .. } => {
            metrics.batches.inc();
            metrics.rows_shipped.inc_by(*rows as u64);
        }
        Outcome::Copied { resync, copied, .. } => {
            metrics.rows_shipped.inc_by(*copied);
            if *resync {
                metrics.resyncs.inc();
                metrics.last_resync_seconds.set(now_secs());
            }
        }
        Outcome::Idle => {}
    }
    if let Some(checkpoint) = checkpoint {
        metrics.epoch.set(i64::from(checkpoint.epoch));
    }
}

/// Set the backlog gauges from SQLite. Also called while sync is failing,
/// so `sync_lag_seconds` keeps growing and the lag alert can fire.
async fn refresh_backlog(metrics: &Metrics, sqlite: &SqlitePool, last_seq: i64) {
    match changes::pending(sqlite, last_seq).await {
        Ok(pending) => {
            metrics.pending_changes.set(pending.count);
            metrics
                .lag_seconds
                .set(lag_seconds(pending.oldest_ms, now_ms()));
        }
        Err(err) => tracing::warn!(
            error = format!("{err:#}"),
            "could not count pending changes"
        ),
    }
}

/// Open the node's database, waiting while the file does not exist yet
/// (`library-web` creates it on a fresh node). `None` means shutdown.
async fn wait_for_sqlite(
    url: &str,
    settings: &Settings,
    shutdown: &CancellationToken,
) -> Option<SqlitePool> {
    let mut logged = false;
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        match sqlite_setup::open_pool(url, SQLITE_CONNECTIONS, false).await {
            Ok(pool) => return Some(pool),
            Err(err) => {
                if !logged {
                    tracing::info!(
                        error = format!("{err:#}"),
                        "waiting for the SQLite database (library-web creates it)"
                    );
                    logged = true;
                }
                sleep_or_cancel(settings.wait_step, shutdown).await;
            }
        }
    }
}

/// Wait until `library-web` has applied every SQLite migration this worker
/// knows (spec §6.7). `false` means shutdown.
async fn wait_for_schema(
    sqlite: &SqlitePool,
    settings: &Settings,
    shutdown: &CancellationToken,
) -> bool {
    let mut logged = false;
    loop {
        if shutdown.is_cancelled() {
            return false;
        }
        match sqlite_schema_ready(sqlite).await {
            Ok(true) => return true,
            Ok(false) if !logged => {
                tracing::info!("waiting for library-web to apply the SQLite migrations");
                logged = true;
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(
                error = format!("{err:#}"),
                "checking the SQLite schema failed"
            ),
        }
        sleep_or_cancel(settings.wait_step, shutdown).await;
    }
}

async fn sleep_or_cancel(duration: Duration, shutdown: &CancellationToken) {
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        () = shutdown.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::production(500)
    }

    #[test]
    fn a_resync_is_counted_and_sets_the_epoch() {
        let m = Metrics::new();
        let checkpoint = Checkpoint {
            epoch: 2,
            last_seq: 0,
            last_token: None,
        };
        let copied = |resync| Outcome::Copied {
            resync,
            epoch: 2,
            archived: 0,
            copied: 4,
        };
        record_outcome(&m, &copied(false), Some(&checkpoint));
        assert_eq!(
            (m.resyncs.get(), m.epoch.get(), m.rows_shipped.get()),
            (0, 2, 4)
        );
        assert_eq!(m.last_resync_seconds.get(), 0.0, "no resync yet");
        record_outcome(&m, &copied(true), Some(&checkpoint));
        assert_eq!((m.resyncs.get(), m.rows_shipped.get()), (1, 8));
        let at = m.last_resync_seconds.get();
        assert!(
            (now_secs() - at).abs() < 5.0,
            "set to the current unix time, got {at}"
        );
    }

    #[test]
    fn a_full_batch_polls_again_at_once() {
        let full = Outcome::Shipped {
            changes: 500,
            rows: 500,
            full: true,
        };
        assert_eq!(
            next_poll(&full, Duration::from_secs(2), &settings()),
            Duration::ZERO
        );
    }

    #[test]
    fn a_partial_batch_or_a_copy_waits_the_minimum() {
        let partial = Outcome::Shipped {
            changes: 3,
            rows: 3,
            full: false,
        };
        assert_eq!(
            next_poll(&partial, Duration::ZERO, &settings()),
            Duration::from_millis(200)
        );
        let copied = Outcome::Copied {
            resync: false,
            epoch: 1,
            archived: 0,
            copied: 0,
        };
        assert_eq!(
            next_poll(&copied, Duration::from_secs(2), &settings()),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn idle_waits_double_from_200ms_up_to_2s() {
        let s = settings();
        let mut wait = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..6 {
            wait = next_poll(&Outcome::Idle, wait, &s);
            seen.push(wait.as_millis());
        }
        assert_eq!(seen, vec![200, 400, 800, 1600, 2000, 2000]);
    }

    #[test]
    fn retries_double_from_1s_up_to_30s() {
        let s = settings();
        let mut wait = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..7 {
            wait = next_retry(wait, &s);
            seen.push(wait.as_secs());
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 30, 30]);
    }
}

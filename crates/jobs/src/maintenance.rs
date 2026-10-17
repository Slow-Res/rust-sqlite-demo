//! The maintenance leader (spec §8.5). Exactly one process holds a session
//! advisory lock on a dedicated connection; if that connection dies, the lock
//! goes with it and another process takes over on its next try. During a
//! hand-over two leaders can overlap briefly: every task below is safe to run
//! twice (SKIP LOCKED reaping, keyed recurring ticks, idempotent deletes).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgConnection, PgPool, Postgres};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::MaintenanceConfig;
use crate::enqueue::Enqueuer;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::store::{maintenance as sql, stats};

/// `pg_try_advisory_lock` key ("jobslead").
pub const LEADER_LOCK: i64 = 0x6A6F_6273_6C65_6164;

/// Upper bound for one maintenance statement or election query: a hung pooled
/// connection must not pin the leader loop. A timed-out step is retried next tick.
const STATEMENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long stepping down may take (unlock + close) before the connection is just dropped.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) struct Leader {
    pub(crate) pool: PgPool,
    #[allow(dead_code)] // used by Task 11
    pub(crate) registry: Arc<Registry>,
    pub(crate) config: MaintenanceConfig,
    #[allow(dead_code)] // used by Task 11
    pub(crate) enqueuer: Enqueuer,
    pub(crate) metrics: Arc<Metrics>,
    /// Live-process window for the gauge: three heartbeats.
    pub(crate) live_within: Duration,
}

pub(crate) async fn run(leader: Leader, stop: CancellationToken) {
    loop {
        if stop.is_cancelled() {
            return;
        }
        // Raced against `stop`, so a stuck election never delays shutdown.
        let elected = tokio::select! {
            biased;
            () = stop.cancelled() => return,
            r = try_lead(&leader.pool) => r,
        };
        match elected {
            Ok(Some(conn)) => {
                lead(&leader, conn, &stop).await;
                leader.metrics.set_leader(false);
                leader.metrics.clear_leader_gauges();
            }
            Ok(None) => {}
            Err(err) => tracing::warn!(error = %err, "jobs: leader election failed"),
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(leader.config.leader_retry) => {}
        }
    }
}

fn timed_out(what: &str) -> sqlx::Error {
    sqlx::Error::Io(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("{what} timed out"),
    ))
}

/// Bound a database step: a hung connection becomes an error instead of a stall.
async fn bounded<T>(
    what: &str,
    limit: Duration,
    fut: impl Future<Output = Result<T, sqlx::Error>>,
) -> Result<T, sqlx::Error> {
    tokio::time::timeout(limit, fut)
        .await
        .unwrap_or_else(|_| Err(timed_out(what)))
}

/// A pooled connection that is closed, not returned to the pool, unless it was
/// released on purpose. `try_lead` can be dropped mid-query (shutdown) after
/// Postgres has already granted the session lock; a plain drop would put that
/// lock-holding session back in the pool and no node could lead until it was recycled.
struct ElectionConn(Option<PoolConnection<Postgres>>);

impl ElectionConn {
    fn conn(&mut self) -> &mut PgConnection {
        // Only `release` and `detach` empty the slot, and both consume `self`.
        self.0.as_mut().map_or_else(
            || unreachable!("election connection used after release"),
            |c| &mut **c,
        )
    }

    /// The lock was definitely not granted: the session may go back to the pool.
    fn release(mut self) {
        drop(self.0.take());
    }

    /// The lock was granted: the session now belongs to the leader.
    fn detach(mut self) -> Option<PgConnection> {
        self.0.take().map(PoolConnection::detach)
    }
}

impl Drop for ElectionConn {
    fn drop(&mut self) {
        // Armed: the outcome is unknown, so close the session (this releases any lock).
        if let Some(conn) = self.0.take() {
            drop(conn.detach());
        }
    }
}

async fn try_lead(pool: &PgPool) -> Result<Option<PgConnection>, sqlx::Error> {
    // Tried on a pooled connection: a failed try holds no lock and goes back to
    // the pool. Only the winner is detached, because the session lock lives as
    // long as that connection and it must never be handed to another user.
    let mut conn = ElectionConn(Some(
        bounded("leader election", STATEMENT_TIMEOUT, pool.acquire()).await?,
    ));
    let got = bounded(
        "leader election",
        STATEMENT_TIMEOUT,
        sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock($1)")
            .bind(LEADER_LOCK)
            .fetch_one(conn.conn()),
    )
    .await?;
    // An error, a timeout or a cancelled future leaves the guard armed: it closes the session.
    if got {
        Ok(conn.detach())
    } else {
        conn.release();
        Ok(None)
    }
}

fn every(d: Duration) -> Interval {
    let mut i = tokio::time::interval(d);
    i.set_missed_tick_behavior(MissedTickBehavior::Delay);
    i
}

/// Resolves with the error when the leader connection stops answering. Bounded
/// by `period` per check, so a half-open connection (no TCP error) is noticed too.
async fn watch(conn: &mut PgConnection, period: Duration) -> sqlx::Error {
    loop {
        tokio::time::sleep(period).await;
        if let Err(err) = bounded(
            "leader liveness check",
            period,
            sqlx::query("SELECT 1").execute(&mut *conn),
        )
        .await
        {
            return err;
        }
    }
}

async fn lead(l: &Leader, mut conn: PgConnection, stop: &CancellationToken) {
    tracing::info!("leader.acquired");
    l.metrics.set_leader(true);
    // The liveness watch runs beside the work loop, not inside it, so a stuck
    // statement cannot hide a lost lock; stop wins over both.
    let lost = tokio::select! {
        biased;
        () = stop.cancelled() => None,
        err = watch(&mut conn, l.config.leader_retry) => Some(err),
        () = work(l) => None,
    };
    if let Some(err) = lost {
        // Never returned to the pool, never closed gracefully (it may not answer).
        // Closing our socket does not end a half-open server session at once, so the lock
        // may linger until TCP keepalive: a leaderless gap, never two leaders.
        tracing::warn!(error = %err, "leader.lost");
        drop(conn);
        return;
    }
    // Stepping down: the lock goes with the connection either way, so do not wait long.
    let release = async {
        let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(LEADER_LOCK)
            .execute(&mut conn)
            .await;
        conn.close().await.ok();
    };
    let _ = tokio::time::timeout(RELEASE_TIMEOUT, release).await;
}

/// The leader's periodic work; runs until the caller drops it. Every step is
/// bounded, so one stuck statement is logged and retried on the next tick.
async fn work(l: &Leader) {
    let c = &l.config;
    let (mut reap, mut prune) = (every(c.reap_every), every(c.prune_every));
    let (mut recurring, mut retention, mut gauges) = (
        every(c.recurring_every),
        every(c.retention_every),
        every(c.gauges_every),
    );
    loop {
        tokio::select! {
            _ = reap.tick() => match bounded("reap", STATEMENT_TIMEOUT, sql::reap(&l.pool)).await {
                Ok(rows) => for r in rows {
                    tracing::warn!(job_id = r.id, cause = %r.cause, state = %r.state, "jobs.reaped");
                },
                Err(err) => { l.metrics.db_error("reap"); tracing::warn!(error = %err, "jobs: reap failed"); }
            },
            _ = prune.tick() => match bounded("prune", STATEMENT_TIMEOUT, sql::prune(&l.pool, c.prune_after)).await {
                Ok((0, _)) => {}
                Ok((processes, jobs)) => tracing::warn!(processes, jobs, "process.pruned"),
                Err(err) => { l.metrics.db_error("prune"); tracing::warn!(error = %err, "jobs: prune failed"); }
            },
            _ = recurring.tick() => run_recurring(l).await,
            _ = retention.tick() => run_retention(l).await,
            _ = gauges.tick() => refresh_gauges(l).await,
        }
    }
}

async fn run_recurring(_l: &Leader) {
    // Task 11.
}

async fn run_retention(_l: &Leader) {
    // Task 11.
}

async fn refresh_gauges(l: &Leader) {
    let result = bounded("gauge refresh", STATEMENT_TIMEOUT, async {
        let queues = stats::queue_stats(&l.pool).await?;
        let failed = stats::failed_recent(&l.pool).await?;
        let live = stats::live_processes(&l.pool, l.live_within).await?;
        Ok((queues, failed, live))
    })
    .await;
    match result {
        Ok((queues, failed, live)) => {
            l.metrics.clear_leader_gauges();
            for q in queues {
                l.metrics.set_queue_gauges(
                    &q.queue,
                    q.available,
                    q.scheduled,
                    q.running,
                    q.oldest_available_s,
                );
            }
            for (kind, n) in failed {
                l.metrics.set_failed_recent(&kind, n);
            }
            l.metrics.set_processes_live(live);
        }
        Err(err) => {
            l.metrics.db_error("gauges");
            tracing::warn!(error = %err, "jobs: gauge refresh failed");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    async fn pool() -> PgPool {
        let url = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL is not set");
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("connect")
    }

    #[tokio::test]
    async fn a_dropped_election_connection_is_closed_not_pooled() {
        let pool = pool().await;
        let conn = ElectionConn(Some(pool.acquire().await.unwrap()));
        let before = pool.size();
        drop(conn);
        assert_eq!(
            pool.num_idle(),
            0,
            "an armed guard must not return the session"
        );
        assert_eq!(pool.size(), before - 1, "the session is closed");
    }

    #[tokio::test]
    async fn a_released_election_connection_goes_back_to_the_pool() {
        let pool = pool().await;
        let conn = ElectionConn(Some(pool.acquire().await.unwrap()));
        conn.release();
        // The pool takes the session back on a background task.
        for _ in 0..100 {
            if pool.num_idle() == 1 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the released session never came back to the pool");
    }
}

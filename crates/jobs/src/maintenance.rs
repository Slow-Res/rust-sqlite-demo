//! The maintenance leader (spec §8.5). Exactly one process holds a session
//! advisory lock on a dedicated connection; if that connection dies, the lock
//! goes with it and another process takes over on its next try. During a
//! hand-over two leaders can overlap briefly: every task below is safe to run
//! twice (SKIP LOCKED reaping, keyed recurring ticks, idempotent deletes).

use std::sync::Arc;
use std::time::Duration;

use sqlx::{Connection, PgConnection, PgPool};
use tokio::time::{Interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::MaintenanceConfig;
use crate::enqueue::Enqueuer;
use crate::metrics::Metrics;
use crate::registry::Registry;
use crate::store::{maintenance as sql, stats};

/// `pg_try_advisory_lock` key ("jobslead").
pub const LEADER_LOCK: i64 = 0x6A6F_6273_6C65_6164;

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
        match try_lead(&leader.pool).await {
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

async fn try_lead(pool: &PgPool) -> Result<Option<PgConnection>, sqlx::Error> {
    // Detached: the lock lives as long as this connection, never back in the pool.
    let mut conn = pool.acquire().await?.detach();
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
        .bind(LEADER_LOCK)
        .fetch_one(&mut conn)
        .await?;
    if got {
        Ok(Some(conn))
    } else {
        conn.close().await.ok();
        Ok(None)
    }
}

fn every(d: Duration) -> Interval {
    let mut i = tokio::time::interval(d);
    i.set_missed_tick_behavior(MissedTickBehavior::Delay);
    i
}

async fn lead(l: &Leader, mut conn: PgConnection, stop: &CancellationToken) {
    tracing::info!("leader.acquired");
    l.metrics.set_leader(true);
    let c = &l.config;
    let (mut check, mut reap, mut prune) = (
        every(c.leader_retry),
        every(c.reap_every),
        every(c.prune_every),
    );
    let (mut recurring, mut retention, mut gauges) = (
        every(c.recurring_every),
        every(c.retention_every),
        every(c.gauges_every),
    );
    loop {
        tokio::select! {
            () = stop.cancelled() => {
                let _ = sqlx::query("SELECT pg_advisory_unlock($1)").bind(LEADER_LOCK).execute(&mut conn).await;
                conn.close().await.ok();
                return;
            }
            _ = check.tick() => {
                if let Err(err) = sqlx::query("SELECT 1").execute(&mut conn).await {
                    tracing::warn!(error = %err, "leader.lost");
                    return;
                }
            }
            _ = reap.tick() => match sql::reap(&l.pool).await {
                Ok(rows) => for r in rows {
                    tracing::warn!(job_id = r.id, cause = %r.cause, state = %r.state, "jobs.reaped");
                },
                Err(err) => { l.metrics.db_error("reap"); tracing::warn!(error = %err, "jobs: reap failed"); }
            },
            _ = prune.tick() => match sql::prune(&l.pool, c.prune_after).await {
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
    let result = async {
        let queues = stats::queue_stats(&l.pool).await?;
        let failed = stats::failed_recent(&l.pool).await?;
        let live = stats::live_processes(&l.pool, l.live_within).await?;
        Ok::<_, sqlx::Error>((queues, failed, live))
    }
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

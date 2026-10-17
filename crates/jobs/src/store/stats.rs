//! Aggregates for the leader's gauges and `jobs status` (spec §17).
//! Scans are capped at 100,000 waiting/running rows so a huge backlog cannot
//! make the query itself a problem (the cap shows as a count of 100,000).

use std::time::Duration;

use sqlx::PgPool;

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct QueueStats {
    pub queue: String,
    pub available: i64,
    pub scheduled: i64,
    pub running: i64,
    pub oldest_available_s: f64,
}

pub async fn queue_stats(pool: &PgPool) -> Result<Vec<QueueStats>, sqlx::Error> {
    sqlx::query_as(
        "SELECT queue,
                count(*) FILTER (WHERE state = 'available' AND run_at <= now()) AS available,
                count(*) FILTER (WHERE state = 'available' AND run_at > now()) AS scheduled,
                count(*) FILTER (WHERE state = 'running') AS running,
                COALESCE(extract(epoch FROM now() - min(run_at) FILTER (WHERE state = 'available' AND run_at <= now())), 0)::float8
                    AS oldest_available_s
         FROM (SELECT queue, state, run_at FROM jobs WHERE state IN ('available', 'running') LIMIT 100000) j
         GROUP BY queue
         ORDER BY queue",
    )
    .fetch_all(pool)
    .await
}

pub async fn failed_recent(pool: &PgPool) -> Result<Vec<(String, i64)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT kind, count(*) FROM jobs
         WHERE state = 'failed' AND finished_at > now() - interval '24 hours'
         GROUP BY kind ORDER BY kind",
    )
    .fetch_all(pool)
    .await
}

pub async fn live_processes(pool: &PgPool, within: Duration) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("SELECT count(*) FROM job_processes WHERE last_heartbeat_at > now() - make_interval(secs => $1::float8)")
        .bind(within.as_secs_f64())
        .fetch_one(pool)
        .await
}

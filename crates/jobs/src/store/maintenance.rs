//! The leader's SQL (spec §8.5, §9.3, §14, §16).

use std::time::Duration;

use sqlx::PgPool;

#[derive(Debug, Clone, sqlx::FromRow, PartialEq, Eq)]
pub struct Reaped {
    pub id: i64,
    pub state: String,
    pub cause: String,
}

/// Expired leases become attempt outcomes. `process_lost` when the holder's
/// registry row is gone or silent for a lease, else `lease_expired`. A crash
/// counts; after `max_crashes` (or the last attempt) the job fails. Crash
/// retries use the default backoff (5 s base, 1 h cap, full jitter).
pub async fn reap(pool: &PgPool) -> Result<Vec<Reaped>, sqlx::Error> {
    sqlx::query_as(
        "WITH expired AS (
             SELECT j.id,
                    CASE WHEN p.id IS NULL OR p.last_heartbeat_at < now() - make_interval(secs => j.lease_s)
                         THEN 'process_lost' ELSE 'lease_expired' END AS cause,
                    (j.crashes + 1 >= j.max_crashes OR j.attempt >= j.max_attempts) AS give_up
             FROM jobs j LEFT JOIN job_processes p ON p.id = j.locked_by
             WHERE j.state = 'running' AND j.locked_until < now()
             ORDER BY j.locked_until
             LIMIT 500
             FOR UPDATE OF j SKIP LOCKED)
         UPDATE jobs j SET
             crashes = j.crashes + 1,
             state = CASE WHEN e.give_up THEN 'failed' ELSE 'available' END,
             finished_at = CASE WHEN e.give_up THEN now() ELSE NULL END,
             run_at = CASE WHEN e.give_up THEN j.run_at
                      ELSE now() + make_interval(secs => random() * least(3600, 5 * power(2, greatest(j.attempt - 1, 0))))
                      END,
             locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(j.errors, jsonb_build_object(
                 'attempt', j.attempt, 'at', now(), 'cause', e.cause,
                 'message', 'the worker stopped renewing the lease'))
         FROM expired e
         WHERE j.id = e.id
         RETURNING j.id, j.state, e.cause",
    )
    .fetch_all(pool)
    .await
}

/// Delete processes silent for `after` and make their running jobs reapable
/// at once. Returns (processes, jobs).
pub async fn prune(pool: &PgPool, after: Duration) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "WITH dead AS (
             DELETE FROM job_processes
             WHERE last_heartbeat_at < now() - make_interval(secs => $1::float8)
             RETURNING id),
         freed AS (
             UPDATE jobs SET locked_until = now() - interval '1 millisecond'
             WHERE state = 'running' AND locked_by IN (SELECT id FROM dead)
             RETURNING id)
         SELECT (SELECT count(*) FROM dead), (SELECT count(*) FROM freed)",
    )
    .bind(after.as_secs_f64())
    .fetch_one(pool)
    .await
}

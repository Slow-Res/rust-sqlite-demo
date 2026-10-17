//! The leader's SQL (spec §8.5, §9.3, §14, §16).

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::enqueue::{EnqueueError, EnqueueOptions, Enqueuer, RawJob};

#[derive(Debug, Clone, sqlx::FromRow, PartialEq, Eq)]
pub struct Reaped {
    pub id: i64,
    pub state: String,
    pub cause: String,
}

/// Expired leases become attempt outcomes. `process_lost` when the holder's
/// registry row is gone or silent for a lease, else `lease_expired`. A crash
/// counts; after `max_crashes` (or the last attempt) the job fails. Crash
/// retries use the default backoff (5 s base, 1 h cap, full jitter). A job
/// whose cancellation was requested is cancelled instead of requeued.
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
             state = CASE WHEN e.give_up THEN 'failed' WHEN j.cancel_requested THEN 'cancelled' ELSE 'available' END,
             finished_at = CASE WHEN e.give_up OR j.cancel_requested THEN now() ELSE NULL END,
             run_at = CASE WHEN e.give_up OR j.cancel_requested THEN j.run_at
                      ELSE now() + make_interval(secs => random() * least(3600, 5 * power(2, least(greatest(j.attempt - 1, 0), 30))))
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

/// Enqueue the current tick of `task` unless some leader already did (spec §14).
/// The tick is the latest multiple of `every` since the epoch, in database time,
/// so missed ticks are never replayed. The `(task, run_at)` key makes this
/// exactly-once even while two leaders overlap after a lock loss.
pub(crate) async fn enqueue_tick(
    pool: &PgPool,
    enqueuer: &Enqueuer,
    task: &str,
    every: Duration,
    raw: &RawJob,
) -> Result<Option<i64>, EnqueueError> {
    let mut tx = pool.begin().await?;
    let tick: DateTime<Utc> = sqlx::query_scalar(
        "SELECT to_timestamp(floor(extract(epoch FROM now()) / $1::float8) * $1::float8)",
    )
    .bind(every.as_secs_f64())
    .fetch_one(&mut *tx)
    .await?;
    let fresh: Option<String> = sqlx::query_scalar(
        "INSERT INTO job_recurring_runs (task, run_at) VALUES ($1, $2) ON CONFLICT DO NOTHING RETURNING task",
    )
    .bind(task)
    .bind(tick)
    .fetch_optional(&mut *tx)
    .await?;
    if fresh.is_none() {
        tx.commit().await?;
        return Ok(None);
    }
    let id = enqueuer
        .insert_raw(&mut tx, raw, &EnqueueOptions::default())
        .await?
        .id();
    sqlx::query("UPDATE job_recurring_runs SET job_id = $3 WHERE task = $1 AND run_at = $2")
        .bind(task)
        .bind(tick)
        .bind(id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(id))
}

/// Batched deletes of finished jobs past their window (spec §16), then
/// recurring-run rows older than 7 days.
pub async fn delete_finished(
    pool: &PgPool,
    keep_succeeded: Duration,
    keep_failed: Duration,
    batch: i64,
) -> Result<u64, sqlx::Error> {
    let mut total = 0;
    for (states, keep) in [
        (vec!["succeeded"], keep_succeeded),
        (vec!["failed", "cancelled"], keep_failed),
    ] {
        loop {
            let n = sqlx::query(
                "DELETE FROM jobs WHERE id IN (
                     SELECT id FROM jobs
                     WHERE state = ANY($1) AND finished_at < now() - make_interval(secs => $2::float8)
                     LIMIT $3)",
            )
            .bind(&states)
            .bind(keep.as_secs_f64())
            .bind(batch)
            .execute(pool)
            .await?
            .rows_affected();
            total += n;
            if i64::try_from(n).unwrap_or(i64::MAX) < batch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    sqlx::query("DELETE FROM job_recurring_runs WHERE created_at < now() - interval '7 days'")
        .execute(pool)
        .await?;
    Ok(total)
}

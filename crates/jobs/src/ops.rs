//! Queries behind `library-web jobs …` (spec §17.4). Counts are capped like
//! the gauges (see `store::stats`), lists are capped at 1,000 rows.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::store::stats::queue_stats;

const MAX_LIST: i64 = 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct QueueStatus {
    pub queue: String,
    pub available: i64,
    pub scheduled: i64,
    pub running: i64,
    pub failed_24h: i64,
    pub oldest_available_s: f64,
    pub paused: bool,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct ProcessRow {
    pub id: i64,
    pub name: String,
    pub kind: String,
    pub hostname: String,
    pub pid: i32,
    pub version: String,
    pub started_at: DateTime<Utc>,
    pub heartbeat_age_s: f64,
    pub stopping: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub queues: Vec<QueueStatus>,
    pub processes: Vec<ProcessRow>,
    /// Kinds with waiting jobs that no live process can run (a missing deploy).
    pub orphan_kinds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct JobSummary {
    pub id: i64,
    pub kind: String,
    pub queue: String,
    pub state: String,
    pub attempt: i32,
    pub max_attempts: i32,
    pub crashes: i32,
    pub run_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct JobDetail {
    #[sqlx(flatten)]
    pub summary: JobSummary,
    pub args: Value,
    pub progress: Option<Value>,
    pub errors: Value,
    pub locked_by: Option<i64>,
    pub locked_until: Option<DateTime<Utc>>,
    pub unique_key: Option<String>,
    pub concurrency_key: Option<String>,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, Default)]
pub struct ListFilter {
    pub state: Option<String>,
    pub queue: Option<String>,
    pub kind: Option<String>,
    pub limit: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// It was waiting; now it is cancelled.
    Cancelled,
    /// It is running; its handler is told on the next heartbeat.
    Requested,
    AlreadyFinished,
    NotFound,
}

/// Columns of [`JobSummary`]; a macro so `concat!` keeps every query a `&'static str`.
macro_rules! summary_columns {
    () => {
        "id, kind, queue, state, attempt, max_attempts, crashes, run_at, created_at, finished_at,
         errors->-1->>'message' AS last_error"
    };
}

pub async fn status(pool: &PgPool) -> Result<Status, sqlx::Error> {
    let stats = queue_stats(pool).await?;
    let paused: Vec<String> = sqlx::query_scalar("SELECT queue FROM job_queue_pauses")
        .fetch_all(pool)
        .await?;
    let failed_by_queue: Vec<(String, i64)> = sqlx::query_as(
        "SELECT queue, count(*) FROM jobs WHERE state = 'failed' AND finished_at > now() - interval '24 hours' GROUP BY queue",
    )
    .fetch_all(pool)
    .await?;
    let mut names: Vec<String> = stats.iter().map(|s| s.queue.clone()).collect();
    names.extend(paused.iter().cloned());
    names.extend(failed_by_queue.iter().map(|(q, _)| q.clone()));
    names.sort();
    names.dedup();
    let queues = names
        .into_iter()
        .map(|queue| {
            let s = stats.iter().find(|s| s.queue == queue);
            QueueStatus {
                available: s.map_or(0, |s| s.available),
                scheduled: s.map_or(0, |s| s.scheduled),
                running: s.map_or(0, |s| s.running),
                oldest_available_s: s.map_or(0.0, |s| s.oldest_available_s),
                failed_24h: failed_by_queue
                    .iter()
                    .find(|(q, _)| *q == queue)
                    .map_or(0, |(_, n)| *n),
                paused: paused.contains(&queue),
                queue,
            }
        })
        .collect();
    let orphan_kinds: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT kind FROM jobs WHERE state = 'available'
         EXCEPT
         SELECT jsonb_array_elements_text(config->'kinds') FROM job_processes
         WHERE last_heartbeat_at > now() - interval '3 minutes'
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(Status {
        queues,
        processes: processes(pool).await?,
        orphan_kinds,
    })
}

pub async fn list(pool: &PgPool, f: &ListFilter) -> Result<Vec<JobSummary>, sqlx::Error> {
    sqlx::query_as(concat!(
        "SELECT ",
        summary_columns!(),
        " FROM jobs
         WHERE ($1::text IS NULL OR state = $1) AND ($2::text IS NULL OR queue = $2) AND ($3::text IS NULL OR kind = $3)
         ORDER BY id DESC LIMIT $4"
    ))
        .bind(&f.state)
        .bind(&f.queue)
        .bind(&f.kind)
        .bind(f.limit.clamp(1, MAX_LIST))
        .fetch_all(pool)
        .await
}

pub async fn show(pool: &PgPool, id: i64) -> Result<Option<JobDetail>, sqlx::Error> {
    sqlx::query_as(concat!(
        "SELECT ",
        summary_columns!(),
        ", args, progress, errors, locked_by, locked_until, unique_key, concurrency_key, cancel_requested
         FROM jobs WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
}

/// Failed or cancelled → available now, with attempts and crashes reset.
/// Returns false when the job is not failed/cancelled, or when another waiting
/// or running job holds its unique key (a finished job gave the key up, so
/// retrying it would violate `jobs_unique`).
pub async fn retry(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = 0, crashes = 0, run_at = now(), finished_at = NULL,
                cancel_requested = false
         WHERE id = $1 AND state IN ('failed', 'cancelled')
           AND (unique_key IS NULL OR NOT EXISTS (
                SELECT 1 FROM jobs o
                WHERE o.kind = jobs.kind AND o.unique_key = jobs.unique_key
                  AND o.state IN ('available', 'running')))",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Every failed job (optionally of one kind, optionally finished within `since`)
/// → available. Rows whose unique key is held by a waiting or running job are
/// skipped, and of several failed rows sharing a key only the newest moves.
pub async fn retry_failed(
    pool: &PgPool,
    kind: Option<&str>,
    since: Option<Duration>,
) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = 0, crashes = 0, run_at = now(), finished_at = NULL,
                cancel_requested = false
         WHERE id IN (
             SELECT DISTINCT ON (f.kind, COALESCE(f.unique_key, f.id::text)) f.id
             FROM jobs f
             WHERE f.state = 'failed' AND ($1::text IS NULL OR f.kind = $1)
               AND ($2::float8 IS NULL OR f.finished_at > now() - make_interval(secs => $2::float8))
               AND (f.unique_key IS NULL OR NOT EXISTS (
                    SELECT 1 FROM jobs o
                    WHERE o.kind = f.kind AND o.unique_key = f.unique_key
                      AND o.state IN ('available', 'running')))
             ORDER BY f.kind, COALESCE(f.unique_key, f.id::text), f.id DESC
         )",
    )
    .bind(kind)
    .bind(since.map(|d| d.as_secs_f64()))
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// A waiting job → cancelled. Running jobs need `cancel`.
pub async fn discard(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = now()
         WHERE id = $1 AND state IN ('available', 'failed')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn cancel(pool: &PgPool, id: i64) -> Result<CancelOutcome, sqlx::Error> {
    let state: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET
             cancel_requested = true,
             state = CASE WHEN state = 'available' THEN 'cancelled' ELSE state END,
             finished_at = CASE WHEN state = 'available' THEN now() ELSE finished_at END
         WHERE id = $1 AND state IN ('available', 'running')
         RETURNING state",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(match state.as_deref() {
        Some("cancelled") => CancelOutcome::Cancelled,
        Some(_) => CancelOutcome::Requested,
        None => {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE id = $1)")
                    .bind(id)
                    .fetch_one(pool)
                    .await?;
            if exists {
                CancelOutcome::AlreadyFinished
            } else {
                CancelOutcome::NotFound
            }
        }
    })
}

pub async fn pause(
    pool: &PgPool,
    queue: &str,
    by: &str,
    reason: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO job_queue_pauses (queue, paused_by, reason) VALUES ($1, $2, $3)
         ON CONFLICT (queue) DO UPDATE SET paused_by = $2, reason = $3, paused_at = now()",
    )
    .bind(queue)
    .bind(by)
    .bind(reason)
    .execute(pool)
    .await
    .map(drop)
}

pub async fn resume(pool: &PgPool, queue: &str) -> Result<bool, sqlx::Error> {
    let r = sqlx::query("DELETE FROM job_queue_pauses WHERE queue = $1")
        .bind(queue)
        .execute(pool)
        .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn processes(pool: &PgPool) -> Result<Vec<ProcessRow>, sqlx::Error> {
    sqlx::query_as(
        "SELECT id, name, kind, hostname, pid, version, started_at,
                extract(epoch FROM now() - last_heartbeat_at)::float8 AS heartbeat_age_s, stopping
         FROM job_processes ORDER BY started_at",
    )
    .fetch_all(pool)
    .await
}

/// What retention would delete now: (succeeded, failed + cancelled).
pub async fn prune_preview(
    pool: &PgPool,
    keep_succeeded: Duration,
    keep_failed: Duration,
) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(
        "SELECT count(*) FILTER (WHERE state = 'succeeded' AND finished_at < now() - make_interval(secs => $1::float8)),
                count(*) FILTER (WHERE state IN ('failed', 'cancelled') AND finished_at < now() - make_interval(secs => $2::float8))
         FROM jobs WHERE state IN ('succeeded', 'failed', 'cancelled')",
    )
    .bind(keep_succeeded.as_secs_f64())
    .bind(keep_failed.as_secs_f64())
    .fetch_one(pool)
    .await
}

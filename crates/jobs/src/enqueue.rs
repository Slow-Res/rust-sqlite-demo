//! The outbox side (spec §6): insert a job with the caller's connection, so it
//! commits or rolls back with the caller's data.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgConnection;

use crate::job::{Job, JobOptions};

/// LISTEN/NOTIFY channel; the payload is the queue name.
pub const NOTIFY_CHANNEL: &str = "jobs";
/// Arguments are small; big data is referenced by id (spec §4.1).
pub const MAX_ARGS_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct EnqueueOptions {
    /// Run no earlier than now + delay (database time).
    pub delay: Option<Duration>,
    /// Overrides the kind's priority.
    pub priority: Option<i16>,
    /// On a waiting duplicate, replace its arguments, concurrency key, delay and
    /// priority (debounce). A duplicate that is waiting out a retry backoff
    /// (already attempted) keeps its `run_at` if that is later than the new delay.
    pub replace: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    New(i64),
    /// A job with the same kind and unique key is waiting or running.
    Duplicate(i64),
}

impl Enqueued {
    pub fn id(self) -> i64 {
        match self {
            Self::New(id) | Self::Duplicate(id) => id,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EnqueueError {
    #[error("encoding the arguments of {kind}: {source}")]
    Encode {
        kind: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("the arguments of {kind} are {bytes} bytes; the limit is {MAX_ARGS_BYTES}")]
    TooLarge { kind: &'static str, bytes: usize },
    #[error("invalid options for {kind}: {reason}")]
    Options { kind: &'static str, reason: String },
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// A job after encoding: what both `enqueue` and recurring tasks insert.
#[derive(Debug, Clone)]
pub(crate) struct RawJob {
    pub(crate) kind: &'static str,
    pub(crate) queue: &'static str,
    pub(crate) args: Value,
    pub(crate) options: JobOptions,
    pub(crate) unique_key: Option<String>,
    pub(crate) concurrency_key: Option<String>,
}

impl RawJob {
    pub(crate) fn encode<J: Job>(job: &J) -> Result<Self, EnqueueError> {
        let options = J::options();
        options.validate().map_err(|reason| EnqueueError::Options {
            kind: J::KIND,
            reason,
        })?;
        let args = serde_json::to_value(job).map_err(|source| EnqueueError::Encode {
            kind: J::KIND,
            source,
        })?;
        let bytes = args.to_string().len();
        if bytes > MAX_ARGS_BYTES {
            return Err(EnqueueError::TooLarge {
                kind: J::KIND,
                bytes,
            });
        }
        Ok(Self {
            kind: J::KIND,
            queue: J::QUEUE,
            args,
            options,
            unique_key: job.unique_key(),
            concurrency_key: job.concurrency_key(),
        })
    }
}

const INSERT: &str = "
INSERT INTO jobs (kind, queue, args, priority, run_at, max_attempts, max_crashes,
                  deadline_s, lease_s, unique_key, concurrency_key)
VALUES ($1, $2, $3, $4, now() + make_interval(secs => $5::float8), $6, $7, $8, $9, $10, $11)
ON CONFLICT (kind, unique_key) WHERE unique_key IS NOT NULL AND state IN ('available', 'running')
DO NOTHING
RETURNING id";

const FIND_DUPLICATE: &str = "
SELECT id, state FROM jobs
WHERE kind = $1 AND unique_key = $2 AND state IN ('available', 'running')";

// The concurrency key is replaced with the arguments: it may depend on fields
// beyond the unique key, and a stale key would let the new arguments run past
// `jobs_one_running_per_key`. A job waiting out a retry backoff (attempt > 0)
// never runs earlier than that backoff says.
const REPLACE: &str = "
UPDATE jobs SET args = $2,
    run_at = CASE WHEN attempt > 0
                  THEN GREATEST(run_at, now() + make_interval(secs => $3::float8))
                  ELSE now() + make_interval(secs => $3::float8) END,
    priority = $4,
    concurrency_key = $5
WHERE id = $1 AND state = 'available'";

/// Inserts jobs. Cheap to clone; hold one in app state.
#[derive(Debug, Clone)]
pub struct Enqueuer {
    notify: bool,
}

impl Enqueuer {
    /// `notify`: send `NOTIFY jobs` so workers wake at once. Off, workers find
    /// jobs on their next poll (spec §6.2: NOTIFY takes a lock at commit).
    pub fn new(notify: bool) -> Self {
        Self { notify }
    }

    /// Insert `job` using `conn`, usually the caller's open transaction.
    pub async fn enqueue<J: Job>(
        &self,
        conn: &mut PgConnection,
        job: &J,
    ) -> Result<Enqueued, EnqueueError> {
        self.enqueue_with(conn, job, EnqueueOptions::default())
            .await
    }

    pub async fn enqueue_with<J: Job>(
        &self,
        conn: &mut PgConnection,
        job: &J,
        opts: EnqueueOptions,
    ) -> Result<Enqueued, EnqueueError> {
        let raw = RawJob::encode(job)?;
        self.insert_raw(conn, &raw, &opts).await
    }

    pub(crate) async fn insert_raw(
        &self,
        conn: &mut PgConnection,
        raw: &RawJob,
        opts: &EnqueueOptions,
    ) -> Result<Enqueued, EnqueueError> {
        let delay_s = opts.delay.map_or(0.0, |d| d.as_secs_f64());
        let priority = opts.priority.unwrap_or(raw.options.priority);
        // A conflicting job can finish between our INSERT and SELECT; then the
        // key is free again and the INSERT is retried. Three rounds are plenty.
        for _ in 0..3 {
            let inserted: Option<i64> = sqlx::query_scalar(INSERT)
                .bind(raw.kind)
                .bind(raw.queue)
                .bind(&raw.args)
                .bind(priority)
                .bind(delay_s)
                .bind(raw.options.max_attempts)
                .bind(raw.options.max_crashes)
                .bind(raw.options.deadline_s())
                .bind(raw.options.lease_s())
                .bind(&raw.unique_key)
                .bind(&raw.concurrency_key)
                .fetch_optional(&mut *conn)
                .await?;
            if let Some(id) = inserted {
                self.notify(conn, raw.queue).await?;
                tracing::debug!(job_id = id, kind = %raw.kind, queue = %raw.queue, "jobs.enqueued");
                return Ok(Enqueued::New(id));
            }
            let Some(key) = &raw.unique_key else {
                // Only a unique-key conflict can suppress the insert.
                return Err(EnqueueError::Db(sqlx::Error::Protocol(
                    "insert returned no row without a unique key".into(),
                )));
            };
            let existing: Option<(i64, String)> = sqlx::query_as(FIND_DUPLICATE)
                .bind(raw.kind)
                .bind(key)
                .fetch_optional(&mut *conn)
                .await?;
            if let Some((id, state)) = existing {
                if opts.replace && state == "available" {
                    sqlx::query(REPLACE)
                        .bind(id)
                        .bind(&raw.args)
                        .bind(delay_s)
                        .bind(priority)
                        .bind(&raw.concurrency_key)
                        .execute(&mut *conn)
                        .await?;
                    self.notify(conn, raw.queue).await?;
                }
                tracing::debug!(job_id = id, kind = %raw.kind, queue = %raw.queue, "jobs.duplicate");
                return Ok(Enqueued::Duplicate(id));
            }
        }
        Err(EnqueueError::Db(sqlx::Error::Protocol(format!(
            "unique job {}:{:?} kept changing state",
            raw.kind, raw.unique_key
        ))))
    }

    async fn notify(&self, conn: &mut PgConnection, queue: &str) -> Result<(), sqlx::Error> {
        if self.notify {
            // Delivered by Postgres only when the surrounding transaction commits.
            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(NOTIFY_CHANNEL)
                .bind(queue)
                .execute(conn)
                .await?;
        }
        Ok(())
    }
}

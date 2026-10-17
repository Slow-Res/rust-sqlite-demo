//! Claiming (spec §7, §12): one short transaction; the handler runs after it
//! commits, so a running job holds no lock and no connection.

use serde_json::Value;
use sqlx::{Acquire, PgPool, Postgres, Transaction};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClaimedJob {
    pub id: i64,
    pub kind: String,
    pub queue: String,
    pub args: Value,
    pub attempt: i32,
    pub max_attempts: i32,
    pub deadline_s: i32,
    pub lease_s: i32,
    /// Seconds between `run_at` and the claim (the `jobs_wait_seconds` metric).
    pub waited_s: f64,
}

#[derive(Debug, Clone)]
pub struct ClaimRequest<'a> {
    pub queue: &'a str,
    /// Only kinds this process can run (spec §7.1: a rolling deploy never
    /// hands a new kind to an old worker).
    pub kinds: &'a [String],
    pub process_id: i64,
    /// Free handler slots for this queue in this process.
    pub limit: i64,
    /// Global cap on running jobs of the queue (spec §7.2).
    pub max_running: Option<i64>,
    /// The shortest lease, in seconds, of any kind this worker runs: the span its
    /// heartbeat logic assumes a handler may keep running after a beat. A row enqueued
    /// with a shorter `lease_s` (an older deploy) gets this lease instead, so the reaper
    /// cannot take it from under a live handler.
    pub min_lease_s: i32,
}

const CLAIM_UNKEYED: &str = "
UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $3,
       locked_until = now() + make_interval(secs => GREATEST(lease_s, $5)), attempt_started_at = now()
WHERE id IN (
    SELECT id FROM jobs
    WHERE state = 'available' AND queue = $1 AND run_at <= now()
      AND kind = ANY($2) AND concurrency_key IS NULL
    ORDER BY priority, run_at, id
    LIMIT $4
    FOR UPDATE SKIP LOCKED)
RETURNING id, kind, queue, args, attempt, max_attempts, deadline_s, lease_s,
          extract(epoch FROM now() - run_at)::float8 AS waited_s";

const PICK_KEYED: &str = "
SELECT id, concurrency_key FROM jobs j
WHERE state = 'available' AND queue = $1 AND run_at <= now() AND kind = ANY($2)
  AND concurrency_key IS NOT NULL AND NOT (concurrency_key = ANY($3))
  AND NOT EXISTS (SELECT 1 FROM jobs r WHERE r.state = 'running' AND r.concurrency_key = j.concurrency_key)
ORDER BY priority, run_at, id
LIMIT 1
FOR UPDATE SKIP LOCKED";

/// Non-blocking per-key lock, held to the end of the transaction. Without it two
/// claimers that take keys `a` and `b` in opposite order each wait inside the
/// unique index for the other's uncommitted row and Postgres reports a deadlock.
const TRY_KEY_LOCK: &str =
    "SELECT pg_try_advisory_xact_lock(hashtextextended('jobs.key.' || $1, 0))";

const CLAIM_ONE: &str = "
UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $2,
       locked_until = now() + make_interval(secs => GREATEST(lease_s, $3)), attempt_started_at = now()
WHERE id = $1
RETURNING id, kind, queue, args, attempt, max_attempts, deadline_s, lease_s,
          extract(epoch FROM now() - run_at)::float8 AS waited_s, concurrency_key";

pub async fn claim(pool: &PgPool, req: &ClaimRequest<'_>) -> Result<Vec<ClaimedJob>, sqlx::Error> {
    if req.limit <= 0 || req.kinds.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await?;
    let paused: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM job_queue_pauses WHERE queue = $1)")
            .bind(req.queue)
            .fetch_one(&mut *tx)
            .await?;
    if paused {
        tx.commit().await?;
        return Ok(Vec::new());
    }
    let mut limit = req.limit;
    if let Some(max) = req.max_running {
        // Serialise claims of this queue only; limited queues are low-throughput.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('jobs.queue.' || $1, 0))")
            .bind(req.queue)
            .execute(&mut *tx)
            .await?;
        let running: i64 =
            sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'running' AND queue = $1")
                .bind(req.queue)
                .fetch_one(&mut *tx)
                .await?;
        limit = limit.min(max - running);
        if limit <= 0 {
            tx.commit().await?;
            return Ok(Vec::new());
        }
    }
    let mut claimed: Vec<ClaimedJob> = sqlx::query_as(CLAIM_UNKEYED)
        .bind(req.queue)
        .bind(req.kinds)
        .bind(req.process_id)
        .bind(limit)
        .bind(req.min_lease_s)
        .fetch_all(&mut *tx)
        .await?;
    claim_keyed(&mut tx, req, limit, &mut claimed).await?;
    tx.commit().await?;
    Ok(claimed)
}

/// Jobs with a concurrency key, one per statement. The advisory lock keeps two
/// claimers from racing for the same key; the unique index
/// `jobs_one_running_per_key` stays the safety net: a violation (the other
/// worker committed between our pick and our lock) means it won, so we skip the
/// key for this round.
async fn claim_keyed(
    tx: &mut Transaction<'_, Postgres>,
    req: &ClaimRequest<'_>,
    limit: i64,
    claimed: &mut Vec<ClaimedJob>,
) -> Result<(), sqlx::Error> {
    let mut skip: Vec<String> = Vec::new();
    let mut tries = 0;
    while (claimed.len() as i64) < limit && tries < limit * 2 {
        tries += 1;
        let Some((id, key)): Option<(i64, String)> = sqlx::query_as(PICK_KEYED)
            .bind(req.queue)
            .bind(req.kinds)
            .bind(&skip)
            .fetch_optional(&mut **tx)
            .await?
        else {
            return Ok(());
        };
        let locked: bool = sqlx::query_scalar(TRY_KEY_LOCK)
            .bind(&key)
            .fetch_one(&mut **tx)
            .await?;
        if !locked {
            skip.push(key);
            continue;
        }
        let mut savepoint = tx.begin().await?;
        let row: Result<(ClaimedJob, Option<String>), sqlx::Error> =
            sqlx::query_as::<_, KeyedRow>(CLAIM_ONE)
                .bind(id)
                .bind(req.process_id)
                .bind(req.min_lease_s)
                .fetch_one(&mut *savepoint)
                .await
                .map(KeyedRow::split);
        match row {
            Ok((job, key)) => {
                savepoint.commit().await?;
                if let Some(key) = key {
                    skip.push(key);
                }
                claimed.push(job);
            }
            Err(err)
                if err
                    .as_database_error()
                    .is_some_and(|e| e.is_unique_violation()) =>
            {
                savepoint.rollback().await?;
                skip.push(key);
            }
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

#[derive(sqlx::FromRow)]
struct KeyedRow {
    #[sqlx(flatten)]
    job: ClaimedJob,
    concurrency_key: Option<String>,
}

impl KeyedRow {
    fn split(self) -> (ClaimedJob, Option<String>) {
        (self.job, self.concurrency_key)
    }
}

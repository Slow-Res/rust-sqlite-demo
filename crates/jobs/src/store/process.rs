//! The process registry (spec §4.2, §8.1-8.2).

use serde_json::Value;
use sqlx::PgPool;

#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub name: String,
    pub kind: String,
    pub hostname: String,
    pub pid: i32,
    pub version: String,
    /// `{queues: [...], kinds: [...]}`: lets `jobs status` find kinds no live process knows.
    pub config: Value,
}

pub async fn register(pool: &PgPool, info: &ProcessInfo) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO job_processes (name, kind, hostname, pid, version, config)
         VALUES ($1, $2, $3, $4, $5, $6) RETURNING id",
    )
    .bind(&info.name)
    .bind(&info.kind)
    .bind(&info.hostname)
    .bind(info.pid)
    .bind(&info.version)
    .bind(&info.config)
    .fetch_one(pool)
    .await
}

/// A running job whose lease the heartbeat renewed.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct LeaseRow {
    pub id: i64,
    pub cancel_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Beat {
    /// The registry row exists. Running ids missing from the list lost their
    /// lease or passed their deadline: their handlers must be cancelled.
    Alive(Vec<LeaseRow>),
    /// The registry row is gone: this process was presumed dead. Self-fence (spec §8.2).
    Pruned,
}

/// One transaction: touch the registry row, then renew the leases of
/// `running_ids` that this process still holds and that are within their deadline.
pub async fn heartbeat(
    pool: &PgPool,
    process_id: i64,
    running_ids: &[i64],
) -> Result<Beat, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let alive: Option<i64> = sqlx::query_scalar(
        "UPDATE job_processes SET last_heartbeat_at = now() WHERE id = $1 RETURNING id",
    )
    .bind(process_id)
    .fetch_optional(&mut *tx)
    .await?;
    if alive.is_none() {
        tx.commit().await?;
        return Ok(Beat::Pruned);
    }
    let rows: Vec<LeaseRow> = if running_ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(
            "UPDATE jobs SET locked_until = now() + make_interval(secs => lease_s)
             WHERE state = 'running' AND locked_by = $1 AND id = ANY($2)
               AND attempt_started_at + make_interval(secs => deadline_s) > now()
             RETURNING id, cancel_requested",
        )
        .bind(process_id)
        .bind(running_ids)
        .fetch_all(&mut *tx)
        .await?
    };
    tx.commit().await?;
    let mut rows = rows;
    rows.sort_by_key(|r| r.id);
    Ok(Beat::Alive(rows))
}

pub async fn set_stopping(pool: &PgPool, process_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE job_processes SET stopping = true WHERE id = $1")
        .bind(process_id)
        .execute(pool)
        .await
        .map(drop)
}

pub async fn deregister(pool: &PgPool, process_id: i64) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM job_processes WHERE id = $1")
        .bind(process_id)
        .execute(pool)
        .await
        .map(drop)
}

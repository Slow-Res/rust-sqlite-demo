//! Shared helpers for the jobs integration tests. Every test gets its own
//! database on the throwaway Postgres from `scripts/test-postgres.sh`.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::future::Future;
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{Connection, PgConnection, PgPool};

/// A fresh database, migrated, plus the URL a child process can use.
pub struct TestDb {
    pub pool: PgPool,
    pub url: String,
    admin: PgConnectOptions,
    name: String,
}

fn base_url() -> String {
    std::env::var("TEST_POSTGRES_URL").expect(
        "TEST_POSTGRES_URL is not set. Run `make test`, or export the URL printed by scripts/test-postgres.sh",
    )
}

fn unique_name() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "jobs_test_{}_{nanos}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

impl TestDb {
    pub async fn new() -> Self {
        let base = base_url();
        let admin: PgConnectOptions = base.parse().expect("valid TEST_POSTGRES_URL");
        let name = unique_name();
        let mut conn = PgConnection::connect_with(&admin)
            .await
            .expect("connect to test Postgres");
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();
        let options = admin.clone().database(&name);
        let pool = PgPoolOptions::new()
            .max_connections(40)
            .connect_with(options)
            .await
            .expect("connect to test database");
        library_jobs::migrate(&pool).await.expect("migrate");
        // Handlers in the tests record each run here.
        sqlx::query("CREATE TABLE test_runs (n BIGINT NOT NULL, attempt INT NOT NULL, at TIMESTAMPTZ NOT NULL DEFAULT now())")
            .execute(&pool)
            .await
            .expect("create test_runs");
        // scripts/test-postgres.sh prints a URL ending in /postgres.
        let (prefix, _) = base.rsplit_once('/').expect("URL has a database path");
        let url = format!("{prefix}/{name}");
        Self {
            pool,
            url,
            admin,
            name,
        }
    }

    pub async fn cleanup(self) {
        self.pool.close().await;
        let mut conn = PgConnection::connect_with(&self.admin)
            .await
            .expect("connect");
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut conn)
        .await
        .expect("drop test database");
        conn.close().await.ok();
    }
}

/// Run `body` with a fresh database; drop it afterwards even if the body panics.
pub async fn with_db<F, Fut>(body: F)
where
    F: FnOnce(PgPool) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    with_db_url(|pool, _url| body(pool)).await;
}

/// Like [`with_db`], also passing the database URL (for child processes).
pub async fn with_db_url<F, Fut>(body: F)
where
    F: FnOnce(PgPool, String) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let db = TestDb::new().await;
    let outcome = tokio::spawn(body(db.pool.clone(), db.url.clone())).await;
    db.cleanup().await;
    if let Err(err) = outcome {
        match err.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(err) => panic!("test task failed: {err}"),
        }
    }
}

/// Poll `check` every 20 ms until it returns true; panic after `timeout`.
pub async fn eventually<F, Fut>(timeout: Duration, what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let start = tokio::time::Instant::now();
    loop {
        if check().await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timed out after {timeout:?} waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `SELECT count(*) …` as i64.
pub async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .expect("count query")
}

// ---------------------------------------------------------------- test job kinds

use library_jobs::{Job, JobContext, JobError};
use serde::{Deserialize, Serialize};

/// Succeeds at once and records its run in `test_runs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Noop {
    pub n: i64,
}

impl Job for Noop {
    const KIND: &'static str = "test_noop.v1";
    const QUEUE: &'static str = "default";
    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        sqlx::query("INSERT INTO test_runs (n, attempt) VALUES ($1, $2)")
            .bind(self.n)
            .bind(ctx.attempt())
            .execute(ctx.pool())
            .await?;
        Ok(())
    }
}

/// De-duplicated by `key`; `v` lets tests see which payload won.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unique {
    pub key: String,
    pub v: i64,
}

impl Job for Unique {
    const KIND: &'static str = "test_unique.v1";
    const QUEUE: &'static str = "default";
    fn unique_key(&self) -> Option<String> {
        Some(self.key.clone())
    }
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        Ok(())
    }
}

/// Arguments larger than the 64 KB limit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Big {
    pub s: String,
}

impl Job for Big {
    const KIND: &'static str = "test_big.v1";
    const QUEUE: &'static str = "default";
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        Ok(())
    }
}

/// Unique by `key`, but its concurrency key follows `v`: a replacing job can
/// carry a different concurrency key than the row it replaces.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyed {
    pub key: String,
    pub v: i64,
}

impl Job for Keyed {
    const KIND: &'static str = "test_keyed.v1";
    const QUEUE: &'static str = "default";
    fn unique_key(&self) -> Option<String> {
        Some(self.key.clone())
    }
    fn concurrency_key(&self) -> Option<String> {
        Some(format!("c{}", self.v))
    }
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        Ok(())
    }
}

// ---------------------------------------------------------------- raw rows

pub async fn register_process(pool: &sqlx::PgPool, name: &str) -> i64 {
    library_jobs::store::process::register(
        pool,
        &library_jobs::store::process::ProcessInfo {
            name: name.to_owned(),
            kind: "worker".into(),
            hostname: "test".into(),
            pid: 1,
            version: "test".into(),
            config: serde_json::json!({ "queues": [], "kinds": [] }),
        },
    )
    .await
    .expect("register process")
}

/// A job already claimed by `process_id`, with a fresh lease.
pub async fn insert_running(
    pool: &sqlx::PgPool,
    process_id: i64,
    kind: &str,
    lease_s: i32,
    deadline_s: i32,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO jobs (kind, queue, args, state, attempt, max_attempts, max_crashes, deadline_s, lease_s,
                           locked_by, locked_until, attempt_started_at)
         VALUES ($1, 'default', '{\"n\": 0}', 'running', 1, 10, 2, $4, $3, $2,
                 now() + make_interval(secs => $3::float8), now())
         RETURNING id",
    )
    .bind(kind)
    .bind(process_id)
    .bind(lease_s)
    .bind(deadline_s)
    .fetch_one(pool)
    .await
    .expect("insert running job")
}

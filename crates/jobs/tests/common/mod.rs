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

/// Claim `job_id` for `process_id` by hand, as `claim` would; returns the attempt.
pub async fn force_running(pool: &sqlx::PgPool, job_id: i64, process_id: i64) -> i32 {
    sqlx::query_scalar(
        "UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $2,
                locked_until = now() + make_interval(secs => lease_s), attempt_started_at = now(),
                finished_at = NULL
         WHERE id = $1 RETURNING attempt",
    )
    .bind(job_id)
    .bind(process_id)
    .fetch_one(pool)
    .await
    .expect("force running")
}

pub async fn new_job(pool: &sqlx::PgPool, kind: &str) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
         VALUES ($1, 'default', '{\"n\": 0}', 10, 2, 900, 60) RETURNING id",
    )
    .bind(kind)
    .fetch_one(pool)
    .await
    .expect("insert job")
}

// ---------------------------------------------------------------- more kinds, registry, config

use std::time::Duration as StdDuration;

/// Fails in the way `mode` says.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fails {
    pub mode: String,
}

impl Job for Fails {
    const KIND: &'static str = "test_fails.v1";
    const QUEUE: &'static str = "default";
    fn options() -> library_jobs::JobOptions {
        library_jobs::JobOptions {
            max_attempts: 3,
            lease: StdDuration::from_secs(1),
            ..Default::default()
        }
    }
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        match self.mode.as_str() {
            "retry" => Err(JobError::retry("try again")),
            "retry_after" => Err(JobError::RetryAfter(
                StdDuration::from_secs(120),
                "slow down".into(),
            )),
            "permanent" => Err(JobError::permanent("bad input")),
            "panic" => panic!("handler panicked"),
            "huge" => Err(JobError::Retry("x".repeat(1_000_000))),
            "cancelled" => Err(JobError::Cancelled),
            other => Err(JobError::permanent(format!("unknown mode {other}"))),
        }
    }
}

/// Sleeps `ms`; with `honour_cancel` it stops when cancelled (and records n = -1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sleepy {
    pub ms: u64,
    pub honour_cancel: bool,
}

impl Job for Sleepy {
    const KIND: &'static str = "test_sleepy.v1";
    const QUEUE: &'static str = "default";
    fn options() -> library_jobs::JobOptions {
        library_jobs::JobOptions {
            lease: StdDuration::from_secs(1),
            ..Default::default()
        }
    }
    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        let sleep = tokio::time::sleep(StdDuration::from_millis(self.ms));
        if self.honour_cancel {
            tokio::select! {
                () = sleep => Ok(()),
                () = ctx.cancelled() => {
                    sqlx::query("INSERT INTO test_runs (n, attempt) VALUES (-1, $1)").bind(ctx.attempt()).execute(ctx.pool()).await?;
                    Err(JobError::Cancelled)
                }
            }
        } else {
            sleep.await;
            Ok(())
        }
    }
}

/// Ignores cancellation and blocks a thread for `ms`; deadline 1 s.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShortDeadline {
    pub ms: u64,
}

impl Job for ShortDeadline {
    const KIND: &'static str = "test_short_deadline.v1";
    const QUEUE: &'static str = "default";
    fn options() -> library_jobs::JobOptions {
        library_jobs::JobOptions {
            deadline: StdDuration::from_secs(1),
            lease: StdDuration::from_secs(1),
            max_attempts: 2,
            ..Default::default()
        }
    }
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        let ms = self.ms;
        tokio::task::spawn_blocking(move || std::thread::sleep(StdDuration::from_millis(ms)))
            .await
            .map_err(JobError::retry)?;
        Ok(())
    }
}

/// Sleeps `ms` on attempt 1 only, then succeeds at once (crash-recovery tests).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SleepOnce {
    pub ms: u64,
}

impl Job for SleepOnce {
    const KIND: &'static str = "test_sleep_once.v1";
    const QUEUE: &'static str = "default";
    fn options() -> library_jobs::JobOptions {
        library_jobs::JobOptions {
            lease: StdDuration::from_secs(1),
            ..Default::default()
        }
    }
    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        if ctx.attempt() == 1 {
            tokio::time::sleep(StdDuration::from_millis(self.ms)).await;
        }
        sqlx::query("INSERT INTO test_runs (n, attempt) VALUES (0, $1)")
            .bind(ctx.attempt())
            .execute(ctx.pool())
            .await?;
        Ok(())
    }
}

pub fn registry() -> library_jobs::Registry {
    library_jobs::Registry::new()
        .register::<Noop>()
        .register::<Unique>()
        .register::<Big>()
        .register::<Fails>()
        .register::<Sleepy>()
        .register::<ShortDeadline>()
        .register::<SleepOnce>()
}

/// Fast settings for tests: heartbeat 200 ms (test kinds use 1 s leases).
pub fn fast_config(queues: &str) -> library_jobs::WorkerConfig {
    let mut c =
        library_jobs::WorkerConfig::new(library_jobs::QueueConfig::parse_list(queues).unwrap());
    c.poll = StdDuration::from_millis(100);
    c.heartbeat = StdDuration::from_millis(200);
    c.shutdown_timeout = StdDuration::from_secs(2);
    c.cancel_grace = StdDuration::from_millis(500);
    c.version = "test".into();
    let m = &mut c.maintenance;
    m.leader_retry = StdDuration::from_millis(200);
    m.reap_every = StdDuration::from_millis(200);
    m.prune_every = StdDuration::from_millis(200);
    m.prune_after = StdDuration::from_millis(1500);
    m.recurring_every = StdDuration::from_millis(100);
    m.retention_every = StdDuration::from_millis(500);
    m.gauges_every = StdDuration::from_millis(200);
    c
}

// ---------------------------------------------------------------- running workers

use std::sync::Arc;

use library_jobs::heartbeat::Health;
use library_jobs::metrics::Metrics;
use library_jobs::worker::{ExitReason, Worker, WorkerError};
use tokio_util::sync::CancellationToken;

pub struct TestWorker {
    pub stop: CancellationToken,
    pub handle: tokio::task::JoinHandle<Result<ExitReason, WorkerError>>,
    pub metrics: Arc<Metrics>,
    pub health: Arc<Health>,
}

pub fn start(pool: &sqlx::PgPool, config: library_jobs::WorkerConfig) -> TestWorker {
    start_with(pool, registry(), config)
}

pub fn start_with(
    pool: &sqlx::PgPool,
    registry: library_jobs::Registry,
    config: library_jobs::WorkerConfig,
) -> TestWorker {
    let worker = Worker::new(pool.clone(), registry, config).expect("valid worker");
    let (metrics, health) = (worker.metrics(), worker.health());
    let stop = CancellationToken::new();
    let handle = tokio::spawn(worker.run(stop.clone()));
    TestWorker {
        stop,
        handle,
        metrics,
        health,
    }
}

impl TestWorker {
    pub async fn shutdown(self) -> ExitReason {
        self.stop.cancel();
        self.handle.await.expect("worker task").expect("worker run")
    }
}

pub async fn enqueue<J: Job>(pool: &sqlx::PgPool, job: &J) -> i64 {
    let mut conn = pool.acquire().await.unwrap();
    library_jobs::Enqueuer::new(true)
        .enqueue(&mut conn, job)
        .await
        .unwrap()
        .id()
}

pub async fn job_state(pool: &sqlx::PgPool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

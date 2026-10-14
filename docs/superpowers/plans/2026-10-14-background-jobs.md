# Background Jobs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Postgres-backed transactional outbox and job queue (`crates/jobs`, crate `library-jobs`) with per-job leases, a process registry, an elected maintenance leader, metrics and an ops CLI, plus the library app's first two jobs (loan confirmation email, signed outgoing webhooks) enqueued inside the borrow/return transactions.

**Architecture:** A generic library crate owns the schema (its own migration table), the enqueue API (takes the caller's `&mut PgConnection`, so the job commits or rolls back with the caller's data), and the worker runtime (claim loops with `FOR UPDATE SKIP LOCKED`, one heartbeat task per process that renews per-job leases, fenced completion writes, an advisory-lock leader for reaping, pruning, recurring jobs, retention and gauges). The web app registers its job types, enqueues from `loans/postgres.rs`, and adds `library-web worker` and `library-web jobs …` subcommands. Everything is behind the web crate's existing `postgres` feature and `JOBS_ENABLED`.

**Tech Stack:** Rust 2024, tokio, sqlx 0.9 (Postgres), axum 0.8, prometheus-client 0.25, tokio-util 0.7, lettre 0.11 (SMTP), reqwest 0.13 (HTTP), hmac 0.13 + sha2 0.11, Postgres 18.6, Mailpit v1.31.2 (test SMTP).

**Spec:** `docs/superpowers/specs/2026-10-14-background-jobs-design.md` (read it first; this plan argues from it). Task 19 records the small deviations listed under Global Constraints in the spec.

## Global Constraints

- Repo `~/rust/rust-sqlite-demo`, branch `feat/jobs`. One commit per task (more if a task says so). Never push, never merge.
- Workspace lints: `unsafe_code = "forbid"`; clippy `unwrap_used` and `expect_used` warn, and CI runs `cargo clippy --workspace --all-targets -- -D warnings`. No `unwrap`/`expect` in library code; test files start with `#![allow(clippy::unwrap_used, clippy::expect_used)]`.
- sqlx 0.9 runtime queries only (`sqlx::query`, `query_as`, `query_scalar` with `&'static str` SQL). No `query!` macros (no `DATABASE_URL` at build time). Dynamic SQL is not needed anywhere in this plan.
- **Database time only.** Every stored time is written with `now()` in SQL; Rust never compares its clock with a stored time. Durations are bound as seconds (`make_interval(secs => $n::float8)`).
- The default `library-web` build must not depend on `sqlx-postgres` (CI checks `cargo tree -p library-web -i sqlx-postgres` is empty). Everything jobs-related in `apps/web` is behind `#[cfg(feature = "postgres")]`, except the plain-data `JobsConfig`.
- Tests needing Postgres use `TEST_POSTGRES_URL` from `scripts/test-postgres.sh` (container `sqlite-production-test-pg`, port 55432). Never stop or remove that container. Each test creates and drops its own database.
- Email tests use `TEST_SMTP_URL` / `TEST_MAILPIT_URL` from the new `scripts/test-mailpit.sh` (container `sqlite-production-test-mailpit`, image `axllent/mailpit:v1.31.2`).
- Comments follow the repo: explain *why*, doc comments on public items, spec section references like `(spec §8.2)`. Committed dev credentials get a `# local only` / `// local only` comment.
- Spec deviations decided while planning (Task 19 writes them into the spec): recurring tasks use a fixed interval (`every`), not cron; no `enqueue_many` (callers loop); no `trace` column; env names carry units (`JOBS_POLL_MS`, `JOBS_SHUTDOWN_TIMEOUT_S`, `JOBS_KEEP_SUCCEEDED_H`, `JOBS_KEEP_FAILED_H`, `JOBS_SMTP_TIMEOUT_S`); the lease is per kind in code (`JobOptions::lease`), not an env var; the migration table is set with `Migrator::dangerous_set_table_name`, not `sqlx.toml`.
- Job kinds and queue names: `loan_email.v1` on queue `email`; `webhook.v1` on queue `webhooks`. Channel for NOTIFY: `jobs`. Migration table: `_jobs_migrations`. Leader advisory lock key: `0x6A6F_6273_6C65_6164_i64`.

## Review Focus

1. **Arguments that no longer decode** (a payload changed without bumping the kind's version): the job must end `failed` with cause `bad_args` and a readable message, and the worker must keep serving other jobs. → Task 7 test `undecodable_args_fail_permanently`.
2. **Enqueueing a unique key while the existing job is running**: returns `Duplicate(existing id)`, creates nothing, never aborts the caller's transaction; once that job finishes, the same key enqueues a new job. → Task 3 test `duplicate_while_running_then_new_after_finish`.
3. **A handler that hangs and ignores cancellation**: after its deadline the attempt ends as `deadline_exceeded` and is retried, and the same worker keeps running other jobs. → Task 9 test `hung_handler_hits_deadline_and_worker_keeps_going`.
4. **A remote that accepts the connection and never answers** (SMTP or webhook endpoint): the attempt fails with a retryable error within the configured timeout and frees its slot. → Task 16 test `hanging_smtp_server_times_out_as_retry`, Task 17 test `hanging_endpoint_times_out_as_retry`.
5. **Huge error messages** (a handler returns a 1 MB error string): stored truncated to 2,000 characters, the errors list capped at 10 entries. → Task 6 test `errors_are_capped_and_truncated`, Task 7 test `huge_error_message_is_truncated`.

---

## File Structure

```
Cargo.toml                                  modify: member crates/jobs, workspace dep library-jobs
crates/jobs/Cargo.toml                      new crate library-jobs
crates/jobs/migrations/20261014000000_jobs.sql   schema (spec §4)
crates/jobs/src/lib.rs                      module list, re-exports, migrate()
crates/jobs/src/backoff.rs                  Backoff (exponential, full jitter)            spec §9.2
crates/jobs/src/config.rs                   QueueConfig, WorkerConfig, MaintenanceConfig  spec §11, §18
crates/jobs/src/rate_limit.rs               per-process token buckets                     spec §12.3
crates/jobs/src/job.rs                      Job trait, JobOptions, JobError               spec §5
crates/jobs/src/context.rs                  JobContext, Deps (type map)                   spec §5, §10
crates/jobs/src/enqueue.rs                  Enqueuer, EnqueueOptions, Enqueued, RawJob     spec §6
crates/jobs/src/store/mod.rs                SQL building blocks (public for tests/ops)
crates/jobs/src/store/process.rs            registry rows, heartbeat SQL                  spec §8.1-8.2
crates/jobs/src/store/claim.rs              claim SQL, limits, concurrency keys            spec §7, §12
crates/jobs/src/store/finish.rs             Fence, fenced outcome writes                  spec §8.3, §9
crates/jobs/src/store/maintenance.rs        reap, prune, recurring tick, retention         spec §8.5, §9.3, §14, §16
crates/jobs/src/store/stats.rs              queue stats (gauges, ops status)               spec §17
crates/jobs/src/registry.rs                 kind -> handler, recurring tasks, validation
crates/jobs/src/executor.rs                 run one claimed job, map outcome, write it     spec §8.4, §9.1
crates/jobs/src/running.rs                  in-memory set of running jobs (tokens)
crates/jobs/src/heartbeat.rs                heartbeat + progress flush task                spec §8.2, §10
crates/jobs/src/worker.rs                   Worker: queue loops, listener, shutdown        spec §11, §13
crates/jobs/src/maintenance.rs              leader election + periodic tasks             spec §8.5
crates/jobs/src/metrics.rs                  prometheus families                           spec §17.2
crates/jobs/src/http.rs                     /metrics /healthz /readyz                      spec §17.3
crates/jobs/src/ops.rs                      queries behind the ops CLI                    spec §17.4
crates/jobs/tests/common/mod.rs             TestDb, fast configs, test job kinds
crates/jobs/tests/{migrate,enqueue,process,claim,finish,execute,worker,maintenance,recurring,http,ops,child_process,outage}.rs
crates/jobs/examples/throughput.rs          ≥ 1,000 no-op jobs/s smoke                    spec §21
apps/web/Cargo.toml                         optional deps under feature postgres
apps/web/src/jobs_config.rs                 JobsConfig (plain data, always compiled)
apps/web/src/config.rs                      Config.jobs
apps/web/src/jobs/mod.rs                    registry(), worker_config(), deps
apps/web/src/jobs/events.rs                 LoanEventKind, LoanEvents (enqueue in tx)      spec §15.1
apps/web/src/jobs/loan_email.rs             LoanEmail job + Mailer                        spec §15.2
apps/web/src/jobs/webhook.rs                Webhook job + WebhookSender + sign()          spec §15.3
apps/web/src/jobs/worker.rs                 `library-web worker`, embedded mode
apps/web/src/jobs/ops_cli.rs                `library-web jobs …` output
apps/web/src/cli.rs, main.rs, lib.rs, state.rs, server.rs, loans/postgres.rs   wiring
apps/web/tests/common/mod.rs                jobs config + migrations in test helpers
apps/web/tests/{jobs_worker,jobs_events,jobs_email,jobs_email_chaos,jobs_webhook}.rs
scripts/test-mailpit.sh                     throwaway Mailpit for tests
Makefile, .github/workflows/ci.yml          Mailpit + web postgres tests
deploy/compose/local/loadtest.web-pg.yml    `worker` service (profile jobs)
docs/JOBS.md, docs/RUNBOOK.md, docs/ARCHITECTURE.md, docs/decisions/0007-own-job-queue.md
```

---

### Task 1: Crate scaffold, schema and test harness

**Files:**
- Modify: `Cargo.toml` (workspace members, `[workspace.dependencies]`)
- Create: `crates/jobs/Cargo.toml`, `crates/jobs/src/lib.rs`, `crates/jobs/migrations/20261014000000_jobs.sql`
- Create: `crates/jobs/tests/common/mod.rs`, `crates/jobs/tests/migrate.rs`

**Interfaces:**
- Produces: `library_jobs::migrate(&PgPool) -> Result<(), sqlx::migrate::MigrateError>`, `library_jobs::MIGRATIONS_TABLE`, the tables `jobs`, `job_processes`, `job_queue_pauses`, `job_recurring_runs`, SQL function `jobs_push_error(jsonb, jsonb)`.
- Produces (tests): `common::TestDb { pool, url }`, `common::with_db(|pool| async {..})`, `common::with_db_url(|pool, url| async {..})`, `common::eventually(timeout, || async { bool })`, table `test_runs(n bigint, attempt int, at timestamptz)` created in every test database.

- [ ] **Step 1: Add the crate to the workspace**

In `Cargo.toml`, change `members` and add the workspace dependency:

```toml
members = ["crates/shared", "crates/sqlite", "crates/jobs", "apps/web", "apps/sync-worker"]
```

and under `[workspace.dependencies]`, after `sqlite-setup = …`:

```toml
library-jobs = { path = "crates/jobs" }
```

Create `crates/jobs/Cargo.toml`:

```toml
[package]
name = "library-jobs"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "library_jobs"
path = "src/lib.rs"

[dependencies]
sqlx = { workspace = true, features = ["postgres", "migrate", "chrono", "json"] }
tokio = { version = "1", features = ["full"] }
tokio-util = { version = "0.7", features = ["rt"] }
serde.workspace = true
serde_json.workspace = true
chrono.workspace = true
rand.workspace = true
thiserror = "2"
anyhow = "1"
tracing = "0.1"
prometheus-client = "0.25"
axum = "0.8"

[dev-dependencies]
tower = { version = "0.5", features = ["util"] }
http-body-util = "0.1"

[lints]
workspace = true
```

- [ ] **Step 2: Write the migration**

Create `crates/jobs/migrations/20261014000000_jobs.sql`:

```sql
-- Background jobs (docs/superpowers/specs/2026-10-14-background-jobs-design.md §4).
-- Owned by crates/jobs; its history is in _jobs_migrations, apart from the app's.
-- All times are database time: workers on several machines never compare clocks.

CREATE TABLE jobs (
    id                 BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    kind               TEXT NOT NULL,          -- versioned, e.g. loan_email.v1
    queue              TEXT NOT NULL,
    args               JSONB NOT NULL,
    state              TEXT NOT NULL DEFAULT 'available'
                       CHECK (state IN ('available', 'running', 'succeeded', 'failed', 'cancelled')),
    priority           SMALLINT NOT NULL DEFAULT 0,   -- lower runs first
    run_at             TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempt            INT NOT NULL DEFAULT 0,        -- times claimed
    max_attempts       INT NOT NULL CHECK (max_attempts >= 1),
    crashes            INT NOT NULL DEFAULT 0,        -- attempts ended by lease expiry
    max_crashes        INT NOT NULL CHECK (max_crashes >= 1),
    deadline_s         INT NOT NULL CHECK (deadline_s >= 1),
    lease_s            INT NOT NULL CHECK (lease_s >= 1),
    locked_by          BIGINT,                        -- job_processes.id (no FK: rows are pruned)
    locked_until       TIMESTAMPTZ,
    attempt_started_at TIMESTAMPTZ,
    unique_key         TEXT,
    concurrency_key    TEXT,
    cancel_requested   BOOLEAN NOT NULL DEFAULT false,
    progress           JSONB,
    errors             JSONB NOT NULL DEFAULT '[]',   -- last 10: {attempt, at, cause, message}
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    finished_at        TIMESTAMPTZ,
    -- A running job always has a holder and a lease, and no other state has them.
    CONSTRAINT jobs_lease_iff_running
        CHECK ((state = 'running') = (locked_by IS NOT NULL AND locked_until IS NOT NULL)),
    CONSTRAINT jobs_finished_iff_terminal
        CHECK ((state IN ('succeeded', 'failed', 'cancelled')) = (finished_at IS NOT NULL))
) WITH (
    -- Same-page updates for the frequent state changes, and vacuum by row count
    -- rather than by table fraction (the setting that cut Postgres CPU in load-test round 18).
    fillfactor = 80,
    autovacuum_vacuum_scale_factor = 0,
    autovacuum_vacuum_threshold = 1000,
    autovacuum_vacuum_insert_scale_factor = 0,
    autovacuum_vacuum_insert_threshold = 1000
);

-- Partial indexes stay small however many finished rows the table holds.
CREATE INDEX jobs_claim ON jobs (queue, priority, run_at, id) WHERE state = 'available';
CREATE INDEX jobs_running_lease ON jobs (locked_until) WHERE state = 'running';
CREATE INDEX jobs_running_holder ON jobs (locked_by) WHERE state = 'running';
CREATE UNIQUE INDEX jobs_unique ON jobs (kind, unique_key)
    WHERE unique_key IS NOT NULL AND state IN ('available', 'running');
CREATE UNIQUE INDEX jobs_one_running_per_key ON jobs (concurrency_key)
    WHERE concurrency_key IS NOT NULL AND state = 'running';
CREATE INDEX jobs_finished ON jobs (finished_at) WHERE state IN ('succeeded', 'failed', 'cancelled');

-- One row per live worker process (spec §4.2, §8).
CREATE TABLE job_processes (
    id                BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    name              TEXT NOT NULL UNIQUE,
    kind              TEXT NOT NULL,      -- worker | embedded
    hostname          TEXT NOT NULL,
    pid               INT NOT NULL,
    version           TEXT NOT NULL,
    config            JSONB NOT NULL,     -- {queues: [...], kinds: [...]}
    started_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_heartbeat_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    stopping          BOOLEAN NOT NULL DEFAULT false
);
CREATE INDEX job_processes_heartbeat ON job_processes (last_heartbeat_at);

CREATE TABLE job_queue_pauses (
    queue     TEXT PRIMARY KEY,
    paused_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    paused_by TEXT NOT NULL,
    reason    TEXT
);

-- One row per recurring tick, so two leaders can never enqueue the same tick (spec §14).
-- Kept 7 days, independently of job retention.
CREATE TABLE job_recurring_runs (
    task       TEXT NOT NULL,
    run_at     TIMESTAMPTZ NOT NULL,
    job_id     BIGINT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (task, run_at)
);

-- Append one error entry and keep the last 10, oldest first.
CREATE FUNCTION jobs_push_error(errs JSONB, e JSONB) RETURNS JSONB
LANGUAGE sql IMMUTABLE AS $$
    SELECT COALESCE(jsonb_agg(x.e ORDER BY x.ord), '[]'::jsonb)
    FROM (
        SELECT t.e, t.ord
        FROM jsonb_array_elements(errs || jsonb_build_array(e)) WITH ORDINALITY AS t(e, ord)
        ORDER BY t.ord DESC
        LIMIT 10
    ) x
$$;
```

- [ ] **Step 3: Write `lib.rs` with `migrate`**

Create `crates/jobs/src/lib.rs`:

```rust
//! Background jobs on Postgres: a transactional outbox plus a job queue with
//! per-job leases, a process registry and an elected maintenance leader.
//! Design: docs/superpowers/specs/2026-10-14-background-jobs-design.md.
//!
//! The app enqueues inside its own transactions ([`Enqueuer`]) and runs
//! [`Worker`]s in a separate process. Delivery is at least once: handlers
//! must be idempotent.

use sqlx::PgPool;

/// The crate's own migration history table, so its versions never mix with the
/// app's `_sqlx_migrations` (spec §3.3).
pub const MIGRATIONS_TABLE: &str = "_jobs_migrations";

/// Create or update the jobs schema. Idempotent and safe to run from every
/// process at start-up (sqlx serialises concurrent runs with an advisory lock).
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
    migrator.run(pool).await
}
```

- [ ] **Step 4: Write the test harness**

Create `crates/jobs/tests/common/mod.rs` (later tasks append to it; keep the sections in this order):

```rust
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
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("jobs_test_{}_{nanos}_{}", std::process::id(), COUNTER.fetch_add(1, Ordering::Relaxed))
}

impl TestDb {
    pub async fn new() -> Self {
        let base = base_url();
        let admin: PgConnectOptions = base.parse().expect("valid TEST_POSTGRES_URL");
        let name = unique_name();
        let mut conn = PgConnection::connect_with(&admin).await.expect("connect to test Postgres");
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
        Self { pool, url, admin, name }
    }

    pub async fn cleanup(self) {
        self.pool.close().await;
        let mut conn = PgConnection::connect_with(&self.admin).await.expect("connect");
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.name)))
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
    sqlx::query_scalar(sql).fetch_one(pool).await.expect("count query")
}
```

- [ ] **Step 5: Write the failing test**

Create `crates/jobs/tests/migrate.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{count, with_db};

#[tokio::test]
async fn migrate_creates_schema_in_its_own_history_table_and_is_idempotent() {
    with_db(|pool| async move {
        // TestDb already migrated once; a second run must be a no-op.
        library_jobs::migrate(&pool).await.unwrap();
        for table in ["jobs", "job_processes", "job_queue_pauses", "job_recurring_runs"] {
            let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(table)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(exists, "{table} missing");
        }
        assert_eq!(count(&pool, "SELECT count(*) FROM _jobs_migrations").await, 1);
        let app_table: bool = sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert!(!app_table, "the jobs migrator must not touch the app's history table");
    })
    .await;
}

#[tokio::test]
async fn push_error_keeps_the_last_ten() {
    with_db(|pool| async move {
        let errs: serde_json::Value = sqlx::query_scalar(
            "SELECT jobs_push_error(
                 (SELECT jsonb_agg(jsonb_build_object('n', i) ORDER BY i) FROM generate_series(1, 10) i),
                 jsonb_build_object('n', 11))",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ns: Vec<i64> = errs.as_array().unwrap().iter().map(|e| e["n"].as_i64().unwrap()).collect();
        assert_eq!(ns, (2..=11).collect::<Vec<i64>>(), "oldest dropped, order kept");
    })
    .await;
}
```

- [ ] **Step 6: Run the tests**

Run: `export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) && cargo test -p library-jobs --test migrate`
Expected: both PASS. (If `migrate!` cannot find `./migrations`, check the path is relative to `crates/jobs/Cargo.toml`.)

- [ ] **Step 7: Lint and commit**

Run: `cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt`
Expected: no warnings.

```bash
git add Cargo.toml Cargo.lock crates/jobs
git commit -m "jobs: crate scaffold, schema in its own migration table, test harness"
```

---

### Task 2: Pure building blocks: backoff, worker config, rate limiter

**Files:**
- Create: `crates/jobs/src/backoff.rs`, `crates/jobs/src/config.rs`, `crates/jobs/src/rate_limit.rs`
- Modify: `crates/jobs/src/lib.rs` (modules + re-exports)

**Interfaces:**
- Produces: `Backoff { base: Duration, cap: Duration }` with `ceiling(attempt: i32) -> Duration`, `delay(attempt: i32) -> Duration`; `QueueConfig { name: String, slots: usize, max_running: Option<i64> }` with `QueueConfig::parse_list(&str) -> Result<Vec<QueueConfig>, ConfigError>`; `WorkerConfig` and `MaintenanceConfig` (fields below) with `WorkerConfig::new(queues)` and `validate()`; `RateLimiter::new()`, `RateLimiter::acquire(&self, key: &str, per_second: f64)`; `ConfigError::Invalid(String)`.

- [ ] **Step 1: Write the failing unit tests**

Create `crates/jobs/src/backoff.rs` with the tests first (implementation in Step 3):

```rust
//! Retry delays: exponential with full jitter (spec §9.2). Jitter spreads the
//! retries of jobs that failed together (an outage) instead of stampeding.

use std::time::Duration;

/// `delay(n)` is uniform in `[0, min(cap, base × 2^(n-1))]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_doubles_from_base_and_stops_at_cap() {
        let b = Backoff::default();
        assert_eq!(b.ceiling(0), Duration::from_secs(5));
        assert_eq!(b.ceiling(1), Duration::from_secs(5));
        assert_eq!(b.ceiling(2), Duration::from_secs(10));
        assert_eq!(b.ceiling(3), Duration::from_secs(20));
        assert_eq!(b.ceiling(11), Duration::from_secs(3600));
        assert_eq!(b.ceiling(i32::MAX), Duration::from_secs(3600));
    }

    #[test]
    fn delay_stays_within_zero_and_ceiling() {
        let b = Backoff { base: Duration::from_secs(1), cap: Duration::from_secs(8) };
        for attempt in 1..=6 {
            for _ in 0..200 {
                let d = b.delay(attempt);
                assert!(d <= b.ceiling(attempt), "{d:?} > ceiling for attempt {attempt}");
            }
        }
    }
}
```

Create `crates/jobs/src/config.rs` with its tests first:

```rust
//! Worker configuration (spec §11, §18). The app parses environment variables
//! into these types; the crate never reads the environment itself.

use std::time::Duration;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
}

/// One queue served by a process: `slots` handlers at once in this process,
/// and optionally at most `max_running` across all processes (spec §7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    pub name: String,
    pub slots: usize,
    pub max_running: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_slots_and_optional_limits() {
        let qs = QueueConfig::parse_list(" email:8, webhooks:16 ,imports:2:2").unwrap();
        assert_eq!(qs, vec![
            QueueConfig { name: "email".into(), slots: 8, max_running: None },
            QueueConfig { name: "webhooks".into(), slots: 16, max_running: None },
            QueueConfig { name: "imports".into(), slots: 2, max_running: Some(2) },
        ]);
    }

    #[test]
    fn rejects_bad_queue_lists() {
        for bad in ["", "email", "email:0", "email:x", "email:2:0", "Email:2", "a:1,a:2", "email:2000", "e mail:1"] {
            assert!(QueueConfig::parse_list(bad).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn worker_config_validation() {
        let mut c = WorkerConfig::new(QueueConfig::parse_list("default:4").unwrap());
        assert!(c.validate().is_ok());
        c.heartbeat = Duration::ZERO;
        assert!(c.validate().is_err());
        let mut c = WorkerConfig::new(vec![]);
        c.maintenance.enabled = false;
        assert!(c.validate().is_err(), "a worker must serve at least one queue");
    }
}
```

Create `crates/jobs/src/rate_limit.rs` with its test first:

```rust
//! Per-process token buckets (spec §12.3). The effective global rate is
//! `per_second × processes`; that is documented, not hidden.

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn bursts_then_paces_per_key() {
        let limiter = RateLimiter::new();
        let start = tokio::time::Instant::now();
        for _ in 0..10 {
            limiter.acquire("host-a", 5.0).await;
        }
        // 5 tokens of burst, then one every 200 ms.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(990) && elapsed <= Duration::from_millis(1010), "{elapsed:?}");
        // Another key has its own bucket.
        let start = tokio::time::Instant::now();
        limiter.acquire("host-b", 5.0).await;
        assert!(start.elapsed() < Duration::from_millis(1));
    }
}
```

Add to `lib.rs` (above `pub const MIGRATIONS_TABLE`):

```rust
pub mod backoff;
pub mod config;
pub mod rate_limit;

pub use backoff::Backoff;
pub use config::{ConfigError, MaintenanceConfig, QueueConfig, WorkerConfig};
pub use rate_limit::RateLimiter;
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p library-jobs --lib`
Expected: compile errors (`Backoff::default`, `ceiling`, `QueueConfig::parse_list`, `WorkerConfig`, `RateLimiter` not found).

- [ ] **Step 3: Implement**

In `backoff.rs`, above the tests:

```rust
impl Default for Backoff {
    fn default() -> Self {
        Self { base: Duration::from_secs(5), cap: Duration::from_secs(3600) }
    }
}

impl Backoff {
    /// The largest delay after the `attempt`-th failure (1-based; 0 counts as 1).
    pub fn ceiling(&self, attempt: i32) -> Duration {
        let doublings = u32::try_from(attempt.saturating_sub(1).max(0)).unwrap_or(0).min(30);
        self.base.saturating_mul(1u32 << doublings).min(self.cap)
    }

    /// A random delay in `[0, ceiling(attempt)]` (full jitter).
    pub fn delay(&self, attempt: i32) -> Duration {
        let max = self.ceiling(attempt).as_secs_f64();
        if max <= 0.0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(rand::random_range(0.0..=max))
    }
}
```

In `config.rs`, above the tests:

```rust
const MAX_SLOTS: usize = 1024;

impl QueueConfig {
    /// Parse `JOBS_QUEUES`: comma-separated `name:slots[:max_running]`, e.g.
    /// `email:8,webhooks:16,imports:2:2`. Names are 1-64 of `a-z 0-9 _ -`.
    pub fn parse_list(raw: &str) -> Result<Vec<QueueConfig>, ConfigError> {
        let mut out: Vec<QueueConfig> = Vec::new();
        for item in raw.split(',').map(str::trim) {
            let parts: Vec<&str> = item.split(':').collect();
            let (name, slots, max) = match parts.as_slice() {
                [n, s] => (*n, *s, None),
                [n, s, m] => (*n, *s, Some(*m)),
                _ => return Err(invalid(format!("queue `{item}` must be name:slots or name:slots:max_running"))),
            };
            let name_ok = !name.is_empty()
                && name.len() <= 64
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !name_ok {
                return Err(invalid(format!("queue name `{name}` must be 1-64 of a-z, 0-9, _ and -")));
            }
            let slots: usize = slots
                .parse()
                .ok()
                .filter(|n| (1..=MAX_SLOTS).contains(n))
                .ok_or_else(|| invalid(format!("slots of `{name}` must be 1-{MAX_SLOTS}, got `{slots}`")))?;
            let max_running = match max {
                None => None,
                Some(m) => Some(
                    m.parse::<i64>()
                        .ok()
                        .filter(|n| (1..=1_000_000).contains(n))
                        .ok_or_else(|| invalid(format!("max_running of `{name}` must be 1-1000000, got `{m}`")))?,
                ),
            };
            if out.iter().any(|q| q.name == name) {
                return Err(invalid(format!("queue `{name}` is listed twice")));
            }
            out.push(QueueConfig { name: name.to_owned(), slots, max_running });
        }
        Ok(out)
    }
}

fn invalid(msg: String) -> ConfigError {
    ConfigError::Invalid(msg)
}

/// The elected leader's periodic work (spec §8.5, §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceConfig {
    /// Take part in leader election. Off only in tests that need a passive worker.
    pub enabled: bool,
    /// How often a non-leader tries the lock, and the leader checks its connection.
    pub leader_retry: Duration,
    pub reap_every: Duration,
    pub prune_every: Duration,
    /// A process silent this long is presumed dead (default 3 × the 60 s lease).
    pub prune_after: Duration,
    pub recurring_every: Duration,
    pub retention_every: Duration,
    pub keep_succeeded: Duration,
    pub keep_failed: Duration,
    pub gauges_every: Duration,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            leader_retry: Duration::from_secs(10),
            reap_every: Duration::from_secs(5),
            prune_every: Duration::from_secs(30),
            prune_after: Duration::from_secs(180),
            recurring_every: Duration::from_secs(1),
            retention_every: Duration::from_secs(60),
            keep_succeeded: Duration::from_secs(24 * 3600),
            keep_failed: Duration::from_secs(14 * 24 * 3600),
            gauges_every: Duration::from_secs(15),
        }
    }
}

/// Everything a [`crate::Worker`] process needs besides the pool and registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    pub queues: Vec<QueueConfig>,
    /// Fallback poll per queue (±10 % jitter); NOTIFY makes it rarely matter.
    pub poll: Duration,
    /// LISTEN for enqueue notifications.
    pub listen: bool,
    /// Heartbeat period; every kind's lease must be at least 3 × this (spec §8.2).
    pub heartbeat: Duration,
    pub shutdown_timeout: Duration,
    /// After cancelling handlers at shutdown, how long to wait for them (spec §13 step 3).
    pub cancel_grace: Duration,
    /// `worker` or `embedded`, stored in the registry row.
    pub process_kind: String,
    /// Crate version + git sha, stored in the registry row.
    pub version: String,
    pub maintenance: MaintenanceConfig,
}

impl WorkerConfig {
    pub fn new(queues: Vec<QueueConfig>) -> Self {
        Self {
            queues,
            poll: Duration::from_secs(1),
            listen: true,
            heartbeat: Duration::from_secs(20),
            shutdown_timeout: Duration::from_secs(25),
            cancel_grace: Duration::from_secs(5),
            process_kind: "worker".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            maintenance: MaintenanceConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.queues.is_empty() {
            return Err(invalid("a worker must serve at least one queue".into()));
        }
        for (what, d) in [("poll", self.poll), ("heartbeat", self.heartbeat)] {
            if d.is_zero() {
                return Err(invalid(format!("{what} must be greater than zero")));
            }
        }
        if self.maintenance.enabled && self.maintenance.prune_after < self.heartbeat * 3 {
            return Err(invalid("prune_after must be at least 3 heartbeats".into()));
        }
        Ok(())
    }
}
```

In `rate_limit.rs`, above the tests:

```rust
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use tokio::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets keyed by a handler-chosen string, e.g. `webhook:example.com`.
/// Capacity (burst) is `max(1, per_second)`.
#[derive(Default)]
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wait until one call for `key` fits `per_second`. Non-positive rates do not limit.
    pub async fn acquire(&self, key: &str, per_second: f64) {
        if per_second <= 0.0 || !per_second.is_finite() {
            return;
        }
        let capacity = per_second.max(1.0);
        loop {
            let wait = {
                let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
                let now = Instant::now();
                let bucket = buckets
                    .entry(key.to_owned())
                    .or_insert(Bucket { tokens: capacity, last: now });
                let refill = now.duration_since(bucket.last).as_secs_f64() * per_second;
                bucket.tokens = (bucket.tokens + refill).min(capacity);
                bucket.last = now;
                if bucket.tokens >= 1.0 {
                    bucket.tokens -= 1.0;
                    return;
                }
                Duration::from_secs_f64((1.0 - bucket.tokens) / per_second)
            };
            tokio::time::sleep(wait).await;
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-jobs --lib`
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: backoff with full jitter, worker config parsing, per-process rate limiter"
```

---

### Task 3: Job trait, context and the transactional enqueue

**Files:**
- Create: `crates/jobs/src/job.rs`, `crates/jobs/src/context.rs`, `crates/jobs/src/enqueue.rs`
- Modify: `crates/jobs/src/lib.rs`, `crates/jobs/tests/common/mod.rs` (test job kinds)
- Test: `crates/jobs/tests/enqueue.rs`

**Interfaces:**
- Consumes: `Backoff`, `RateLimiter` (Task 2).
- Produces:
  - `trait Job: Serialize + DeserializeOwned + Send + Sync + 'static { const KIND: &'static str; const QUEUE: &'static str; fn options() -> JobOptions; fn unique_key(&self) -> Option<String>; fn concurrency_key(&self) -> Option<String>; fn perform(self, ctx: JobContext) -> impl Future<Output = Result<(), JobError>> + Send; }`
  - `JobOptions { max_attempts: i32, max_crashes: i32, deadline: Duration, lease: Duration, priority: i16, backoff: Backoff }` (+ `validate() -> Result<(), String>`, `deadline_s() -> i32`, `lease_s() -> i32`)
  - `enum JobError { Retry(String), RetryAfter(Duration, String), Permanent(String), Cancelled }` with `JobError::retry(impl Display)`, `JobError::permanent(impl Display)`, `From<anyhow::Error>`, `From<sqlx::Error>`
  - `JobContext` (Clone) with `pool()`, `id()`, `attempt()`, `kind()`, `idempotency_key()`, `cancelled()`, `is_cancelled()`, `set_progress(Value)`, `rate_limit(&str, f64)`, `require::<T>() -> Result<Arc<T>, JobError>`; `JobContext::detached(pool, id, attempt, kind, deps)` for running a handler outside a worker; `pub(crate) JobContext::new(...)`
  - `Deps` (type map): `Deps::new()`, `insert<T: Send + Sync + 'static>(&mut self, Arc<T>)`, `get<T>() -> Option<Arc<T>>`
  - `Enqueuer::new(notify: bool)`, `enqueue<J: Job>(&self, &mut PgConnection, &J) -> Result<Enqueued, EnqueueError>`, `enqueue_with(…, EnqueueOptions)`; `EnqueueOptions { delay: Option<Duration>, priority: Option<i16>, replace: bool }`; `enum Enqueued { New(i64), Duplicate(i64) }` with `id()`; `EnqueueError::{Encode, TooLarge, Options, Db}`; `pub const NOTIFY_CHANNEL: &str = "jobs"`; `pub(crate) struct RawJob` + `RawJob::encode<J>()` + `Enqueuer::insert_raw(&mut PgConnection, &RawJob, &EnqueueOptions)`.
- Produces (tests): job kinds in `common`: `Noop { n }`, `Unique { key, v }`, `Big { s }`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/jobs/tests/common/mod.rs`:

```rust
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
```

Add `serde = { workspace = true }` to `[dev-dependencies]` of `crates/jobs/Cargo.toml` if the test crate cannot see it (it is already a normal dependency, so it should resolve; add only if the compiler complains).

Create `crates/jobs/tests/enqueue.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::{Big, Noop, Unique, count, with_db};
use library_jobs::{EnqueueError, EnqueueOptions, Enqueued, Enqueuer};
use sqlx::postgres::PgListener;

#[tokio::test]
async fn commit_makes_the_job_visible_and_rollback_discards_it() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(true);
        let mut tx = pool.begin().await.unwrap();
        enqueuer.enqueue(&mut tx, &Noop { n: 1 }).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);

        let mut tx = pool.begin().await.unwrap();
        let id = enqueuer.enqueue(&mut tx, &Noop { n: 2 }).await.unwrap().id();
        tx.commit().await.unwrap();
        let (kind, queue, state, args, attempt, max_attempts, deadline_s, lease_s): (
            String, String, String, serde_json::Value, i32, i32, i32, i32,
        ) = sqlx::query_as(
            "SELECT kind, queue, state, args, attempt, max_attempts, deadline_s, lease_s FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((kind.as_str(), queue.as_str(), state.as_str()), ("test_noop.v1", "default", "available"));
        assert_eq!(args, serde_json::json!({ "n": 2 }));
        assert_eq!((attempt, max_attempts, deadline_s, lease_s), (0, 10, 900, 60));
    })
    .await;
}

#[tokio::test]
async fn notify_arrives_only_after_commit() {
    with_db(|pool| async move {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(library_jobs::NOTIFY_CHANNEL).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        Enqueuer::new(true).enqueue(&mut tx, &Noop { n: 1 }).await.unwrap();
        let early = tokio::time::timeout(Duration::from_millis(300), listener.recv()).await;
        assert!(early.is_err(), "notification delivered before commit");
        tx.commit().await.unwrap();
        let note = tokio::time::timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("notification after commit")
            .unwrap();
        assert_eq!(note.payload(), "default");
    })
    .await;
}

#[tokio::test]
async fn no_notification_when_disabled() {
    with_db(|pool| async move {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(library_jobs::NOTIFY_CHANNEL).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        Enqueuer::new(false).enqueue(&mut conn, &Noop { n: 1 }).await.unwrap();
        let got = tokio::time::timeout(Duration::from_millis(300), listener.recv()).await;
        assert!(got.is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_unique_enqueues_create_one_job_and_never_fail() {
    with_db(|pool| async move {
        let mut tasks = Vec::new();
        for v in 0..16 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                let r = Enqueuer::new(true)
                    .enqueue(&mut tx, &Unique { key: "k".into(), v })
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                r
            }));
        }
        let mut new = Vec::new();
        let mut dup = Vec::new();
        for t in tasks {
            match t.await.unwrap() {
                Enqueued::New(id) => new.push(id),
                Enqueued::Duplicate(id) => dup.push(id),
            }
        }
        assert_eq!(new.len(), 1);
        assert_eq!(dup.len(), 15);
        assert!(dup.iter().all(|id| *id == new[0]));
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    })
    .await;
}

#[tokio::test]
async fn duplicate_while_running_then_new_after_finish() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(false);
        let mut conn = pool.acquire().await.unwrap();
        let first = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 1 }).await.unwrap().id();
        sqlx::query("UPDATE jobs SET state = 'running', locked_by = 1, locked_until = now() + interval '1 hour' WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let again = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 2 }).await.unwrap();
        assert_eq!(again, Enqueued::Duplicate(first));
        sqlx::query("UPDATE jobs SET state = 'succeeded', locked_by = NULL, locked_until = NULL, finished_at = now() WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let later = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 3 }).await.unwrap();
        assert!(matches!(later, Enqueued::New(id) if id != first));
    })
    .await;
}

#[tokio::test]
async fn replace_updates_a_waiting_duplicate_but_not_a_running_one() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(false);
        let mut conn = pool.acquire().await.unwrap();
        let replace = EnqueueOptions { replace: true, ..Default::default() };
        let id = enqueuer.enqueue(&mut conn, &Unique { key: "d".into(), v: 1 }).await.unwrap().id();
        let r = enqueuer.enqueue_with(&mut conn, &Unique { key: "d".into(), v: 2 }, replace.clone()).await.unwrap();
        assert_eq!(r, Enqueued::Duplicate(id));
        let v: i64 = sqlx::query_scalar("SELECT (args->>'v')::bigint FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(v, 2);
        sqlx::query("UPDATE jobs SET state = 'running', locked_by = 1, locked_until = now() + interval '1 hour' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        enqueuer.enqueue_with(&mut conn, &Unique { key: "d".into(), v: 3 }, replace).await.unwrap();
        let v: i64 = sqlx::query_scalar("SELECT (args->>'v')::bigint FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(v, 2, "a running job's arguments must not change");
    })
    .await;
}

#[tokio::test]
async fn oversized_arguments_are_rejected_before_any_write() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let err = Enqueuer::new(false)
            .enqueue(&mut conn, &Big { s: "x".repeat(70 * 1024) })
            .await
            .unwrap_err();
        assert!(matches!(err, EnqueueError::TooLarge { .. }), "{err}");
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);
    })
    .await;
}

#[tokio::test]
async fn delay_and_priority_are_stored() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let opts = EnqueueOptions { delay: Some(Duration::from_secs(60)), priority: Some(5), replace: false };
        let id = Enqueuer::new(false).enqueue_with(&mut conn, &Noop { n: 1 }, opts).await.unwrap().id();
        let (in_future, priority): (bool, i16) = sqlx::query_as(
            "SELECT run_at > now() + interval '59 seconds', priority FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(in_future);
        assert_eq!(priority, 5);
    })
    .await;
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p library-jobs --test enqueue`
Expected: compile errors (`Job`, `JobContext`, `Enqueuer`, … not found).

- [ ] **Step 3: Implement `job.rs`**

```rust
//! What a job is (spec §5): a serde payload with a versioned kind, a queue,
//! options, and an async `perform`.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::backoff::Backoff;
use crate::context::JobContext;

/// A background job. `KIND` carries a version suffix (`loan_email.v1`): change
/// the payload, change the version, so old and new workers can coexist during
/// a deploy (spec §4.1, §19).
///
/// Delivery is at least once: `perform` may run again after a crash, so it
/// must be idempotent (use [`JobContext::idempotency_key`] towards remotes).
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    const KIND: &'static str;
    const QUEUE: &'static str;

    fn options() -> JobOptions {
        JobOptions::default()
    }

    /// Jobs with the same kind and key are de-duplicated while waiting or running (spec §6.3).
    fn unique_key(&self) -> Option<String> {
        None
    }

    /// At most one running job per key, across all processes (spec §12.2).
    fn concurrency_key(&self) -> Option<String> {
        None
    }

    fn perform(self, ctx: JobContext) -> impl Future<Output = Result<(), JobError>> + Send;
}

/// Per-kind defaults, stored on each job row at enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOptions {
    pub max_attempts: i32,
    /// Lease expiries tolerated before the job fails (spec §9.3).
    pub max_crashes: i32,
    /// Longest a single attempt may run (spec §8.4).
    pub deadline: Duration,
    /// Lease length; must be at least 3 heartbeats (validated by the registry).
    pub lease: Duration,
    pub priority: i16,
    pub backoff: Backoff,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            max_crashes: 2,
            deadline: Duration::from_secs(15 * 60),
            lease: Duration::from_secs(60),
            priority: 0,
            backoff: Backoff::default(),
        }
    }
}

impl JobOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_attempts < 1 {
            return Err("max_attempts must be at least 1".into());
        }
        if self.max_crashes < 1 {
            return Err("max_crashes must be at least 1".into());
        }
        if self.deadline < Duration::from_secs(1) || self.lease < Duration::from_secs(1) {
            return Err("deadline and lease must be at least 1 s".into());
        }
        if self.deadline.as_secs() > i32::MAX as u64 || self.lease.as_secs() > i32::MAX as u64 {
            return Err("deadline and lease must fit in i32 seconds".into());
        }
        Ok(())
    }

    /// Whole seconds, rounded up, as stored in `jobs.deadline_s`.
    pub fn deadline_s(&self) -> i32 {
        secs_ceil(self.deadline)
    }

    pub fn lease_s(&self) -> i32 {
        secs_ceil(self.lease)
    }
}

fn secs_ceil(d: Duration) -> i32 {
    let s = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    i32::try_from(s.max(1)).unwrap_or(i32::MAX)
}

/// How an attempt failed (spec §5, §9.1).
#[derive(Debug)]
pub enum JobError {
    /// Try again after the kind's backoff.
    Retry(String),
    /// Try again, but not before this delay (e.g. HTTP 429 `Retry-After`).
    RetryAfter(Duration, String),
    /// Never retry: bad input, a 4xx answer, a missing dependency.
    Permanent(String),
    /// The handler stopped because `ctx.cancelled()` fired.
    Cancelled,
}

impl JobError {
    pub fn retry(err: impl fmt::Display) -> Self {
        Self::Retry(err.to_string())
    }

    pub fn permanent(err: impl fmt::Display) -> Self {
        Self::Permanent(err.to_string())
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry(m) => write!(f, "retryable: {m}"),
            Self::RetryAfter(d, m) => write!(f, "retry after {d:?}: {m}"),
            Self::Permanent(m) => write!(f, "permanent: {m}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// `?` on an `anyhow` error means "retry".
impl From<anyhow::Error> for JobError {
    fn from(err: anyhow::Error) -> Self {
        Self::Retry(format!("{err:#}"))
    }
}

impl From<sqlx::Error> for JobError {
    fn from(err: sqlx::Error) -> Self {
        Self::Retry(err.to_string())
    }
}
```

- [ ] **Step 4: Implement `context.rs`**

```rust
//! What a handler gets besides its payload (spec §5, §10).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::job::JobError;
use crate::rate_limit::RateLimiter;

/// A type map of shared dependencies (an SMTP transport, an HTTP client),
/// provided once to the registry and read by handlers with [`JobContext::require`].
#[derive(Default, Clone)]
pub struct Deps {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Deps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.map.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map.get(&TypeId::of::<T>()).cloned().and_then(|v| v.downcast::<T>().ok())
    }
}

#[derive(Clone)]
pub struct JobContext {
    pool: PgPool,
    id: i64,
    attempt: i32,
    kind: Arc<str>,
    token: CancellationToken,
    progress: Arc<Mutex<Option<Value>>>,
    limiter: Arc<RateLimiter>,
    deps: Arc<Deps>,
}

impl JobContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        pool: PgPool,
        id: i64,
        attempt: i32,
        kind: Arc<str>,
        token: CancellationToken,
        progress: Arc<Mutex<Option<Value>>>,
        limiter: Arc<RateLimiter>,
        deps: Arc<Deps>,
    ) -> Self {
        Self { pool, id, attempt, kind, token, progress, limiter, deps }
    }

    /// A context outside any worker, to call a handler directly (tests, one-off
    /// scripts). Nothing it reports is written anywhere.
    pub fn detached(pool: PgPool, id: i64, attempt: i32, kind: &str, deps: Deps) -> Self {
        Self::new(
            pool,
            id,
            attempt,
            Arc::from(kind),
            CancellationToken::new(),
            Arc::new(Mutex::new(None)),
            Arc::new(RateLimiter::new()),
            Arc::new(deps),
        )
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn id(&self) -> i64 {
        self.id
    }

    /// 1 on the first run.
    pub fn attempt(&self) -> i32 {
        self.attempt
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// `"<kind>:<job id>"`: the same on every attempt, for remote de-duplication.
    pub fn idempotency_key(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }

    /// Fires when an operator cancels the job, the lease is lost, or the worker shuts down.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.token.cancelled()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Report progress; the worker writes the latest value at most once per second (spec §10).
    pub fn set_progress(&self, value: Value) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
    }

    /// Wait for a token of this process's bucket `key` (spec §12.3).
    pub async fn rate_limit(&self, key: &str, per_second: f64) {
        self.limiter.acquire(key, per_second).await;
    }

    /// A dependency provided to the registry, or a permanent error naming it.
    pub fn require<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, JobError> {
        self.deps.get::<T>().ok_or_else(|| {
            JobError::Permanent(format!("missing dependency {}", std::any::type_name::<T>()))
        })
    }
}
```

- [ ] **Step 5: Implement `enqueue.rs`**

```rust
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
    /// On a waiting duplicate, replace its arguments, delay and priority (debounce).
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
        options
            .validate()
            .map_err(|reason| EnqueueError::Options { kind: J::KIND, reason })?;
        let args = serde_json::to_value(job).map_err(|source| EnqueueError::Encode { kind: J::KIND, source })?;
        let bytes = args.to_string().len();
        if bytes > MAX_ARGS_BYTES {
            return Err(EnqueueError::TooLarge { kind: J::KIND, bytes });
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

const REPLACE: &str = "
UPDATE jobs SET args = $2, run_at = now() + make_interval(secs => $3::float8), priority = $4
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
    pub async fn enqueue<J: Job>(&self, conn: &mut PgConnection, job: &J) -> Result<Enqueued, EnqueueError> {
        self.enqueue_with(conn, job, EnqueueOptions::default()).await
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
                        .execute(&mut *conn)
                        .await?;
                    self.notify(conn, raw.queue).await?;
                }
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
```

Add to `lib.rs`:

```rust
pub mod context;
pub mod enqueue;
pub mod job;

pub use context::{Deps, JobContext};
pub use enqueue::{EnqueueError, EnqueueOptions, Enqueued, Enqueuer, MAX_ARGS_BYTES, NOTIFY_CHANNEL};
pub use job::{Job, JobError, JobOptions};
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p library-jobs --test enqueue`
Expected: 8 PASS.

- [ ] **Step 7: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: Job trait, context, and transactional enqueue with de-duplication and NOTIFY on commit"
```

---
### Task 4: Process registry and heartbeat SQL

**Files:**
- Create: `crates/jobs/src/store/mod.rs`, `crates/jobs/src/store/process.rs`
- Modify: `crates/jobs/src/lib.rs`, `crates/jobs/tests/common/mod.rs`
- Test: `crates/jobs/tests/process.rs`

**Interfaces:**
- Produces: `store::process::ProcessInfo { name, kind, hostname, pid: i32, version, config: Value }`, `register(&PgPool, &ProcessInfo) -> Result<i64, sqlx::Error>`, `heartbeat(&PgPool, process_id: i64, running_ids: &[i64]) -> Result<Beat, sqlx::Error>`, `enum Beat { Alive(Vec<LeaseRow>), Pruned }`, `LeaseRow { id: i64, cancel_requested: bool }`, `set_stopping(&PgPool, i64)`, `deregister(&PgPool, i64)`.
- Produces (tests): `common::register_process(&PgPool, name) -> i64`, `common::insert_running(&PgPool, process_id, kind, lease_s, deadline_s) -> i64`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/jobs/tests/common/mod.rs`:

```rust
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
pub async fn insert_running(pool: &sqlx::PgPool, process_id: i64, kind: &str, lease_s: i32, deadline_s: i32) -> i64 {
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
```

Create `crates/jobs/tests/process.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{insert_running, register_process, with_db};
use library_jobs::store::process::{Beat, deregister, heartbeat, set_stopping};

#[tokio::test]
async fn heartbeat_renews_only_this_process_jobs_within_their_deadline() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let other = register_process(&pool, "other").await;
        let mine = insert_running(&pool, me, "k", 60, 900).await;
        let theirs = insert_running(&pool, other, "k", 60, 900).await;
        let overdue = insert_running(&pool, me, "k", 60, 60).await;
        sqlx::query("UPDATE jobs SET attempt_started_at = now() - interval '2 minutes', locked_until = now() + interval '1 second' WHERE id = ANY($1)")
            .bind(vec![mine, theirs, overdue])
            .execute(&pool)
            .await
            .unwrap();
        // `mine` is past 2 min of a 900 s deadline: fine. `overdue` is past its 60 s deadline.
        let beat = heartbeat(&pool, me, &[mine, theirs, overdue]).await.unwrap();
        let Beat::Alive(rows) = beat else { panic!("pruned") };
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![mine]);
        let renewed: bool = sqlx::query_scalar("SELECT locked_until > now() + interval '59 seconds' FROM jobs WHERE id = $1")
            .bind(mine).fetch_one(&pool).await.unwrap();
        assert!(renewed);
        let untouched: bool = sqlx::query_scalar("SELECT locked_until < now() + interval '2 seconds' FROM jobs WHERE id = $1")
            .bind(theirs).fetch_one(&pool).await.unwrap();
        assert!(untouched, "another process's lease must not move");
    })
    .await;
}

#[tokio::test]
async fn heartbeat_reports_cancel_requests_and_updates_the_registry() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let job = insert_running(&pool, me, "k", 60, 900).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 minute'").execute(&pool).await.unwrap();
        sqlx::query("UPDATE jobs SET cancel_requested = true WHERE id = $1").bind(job).execute(&pool).await.unwrap();
        let Beat::Alive(rows) = heartbeat(&pool, me, &[job]).await.unwrap() else { panic!("pruned") };
        assert!(rows[0].cancel_requested);
        let fresh: bool = sqlx::query_scalar("SELECT last_heartbeat_at > now() - interval '1 second' FROM job_processes WHERE id = $1")
            .bind(me).fetch_one(&pool).await.unwrap();
        assert!(fresh);
    })
    .await;
}

#[tokio::test]
async fn a_deleted_registry_row_reads_as_pruned() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        set_stopping(&pool, me).await.unwrap();
        deregister(&pool, me).await.unwrap();
        assert!(matches!(heartbeat(&pool, me, &[]).await.unwrap(), Beat::Pruned));
    })
    .await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test process`
Expected: compile error (`store` not found).

- [ ] **Step 3: Implement**

Create `crates/jobs/src/store/mod.rs`:

```rust
//! The worker's SQL, one module per concern. Public so the integration tests
//! and the ops tooling can drive each step on its own; not a stable API.

pub mod process;
```

Create `crates/jobs/src/store/process.rs`:

```rust
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
pub async fn heartbeat(pool: &PgPool, process_id: i64, running_ids: &[i64]) -> Result<Beat, sqlx::Error> {
    let mut tx = pool.begin().await?;
    let alive: Option<i64> =
        sqlx::query_scalar("UPDATE job_processes SET last_heartbeat_at = now() WHERE id = $1 RETURNING id")
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
```

Add `pub mod store;` to `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-jobs --test process`
Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: process registry and one-statement heartbeat that renews per-job leases"
```

---

### Task 5: Claiming (SKIP LOCKED, pauses, queue limits, concurrency keys)

**Files:**
- Create: `crates/jobs/src/store/claim.rs`
- Modify: `crates/jobs/src/store/mod.rs`
- Test: `crates/jobs/tests/claim.rs`

**Interfaces:**
- Produces: `store::claim::ClaimedJob { id: i64, kind: String, queue: String, args: Value, attempt: i32, max_attempts: i32, deadline_s: i32, lease_s: i32, waited_s: f64 }`, `ClaimRequest<'a> { queue: &'a str, kinds: &'a [String], process_id: i64, limit: i64, max_running: Option<i64> }`, `claim(&PgPool, &ClaimRequest<'_>) -> Result<Vec<ClaimedJob>, sqlx::Error>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/jobs/tests/claim.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{count, register_process, with_db};
use library_jobs::store::claim::{ClaimRequest, claim};
use sqlx::PgPool;

async fn add(pool: &PgPool, queue: &str, kind: &str, priority: i16, delay_s: f64, key: Option<&str>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO jobs (kind, queue, args, priority, run_at, max_attempts, max_crashes, deadline_s, lease_s, concurrency_key)
         VALUES ($1, $2, '{}', $3, now() + make_interval(secs => $4::float8), 10, 2, 900, 60, $5) RETURNING id",
    )
    .bind(kind).bind(queue).bind(priority).bind(delay_s).bind(key)
    .fetch_one(pool).await.unwrap()
}

fn kinds() -> Vec<String> {
    vec!["k".to_owned()]
}

#[tokio::test]
async fn claims_by_priority_then_age_up_to_the_limit_and_sets_the_lease() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let low = add(&pool, "q", "k", 5, -10.0, None).await;
        let old = add(&pool, "q", "k", 0, -20.0, None).await;
        let new = add(&pool, "q", "k", 0, -1.0, None).await;
        let kinds = kinds();
        let req = ClaimRequest { queue: "q", kinds: &kinds, process_id: me, limit: 2, max_running: None };
        let mut got: Vec<i64> = claim(&pool, &req).await.unwrap().into_iter().map(|j| j.id).collect();
        got.sort();
        let mut want = vec![old, new];
        want.sort();
        assert_eq!(got, want, "priority 0 before 5; limit 2");
        let (attempt, leased, started): (i32, bool, bool) = sqlx::query_as(
            "SELECT attempt, locked_until BETWEEN now() + interval '59 s' AND now() + interval '61 s',
                    attempt_started_at IS NOT NULL FROM jobs WHERE id = $1",
        ).bind(old).fetch_one(&pool).await.unwrap();
        assert_eq!((attempt, leased, started), (1, true, true));
        let rest = claim(&pool, &req).await.unwrap();
        assert_eq!(rest.iter().map(|j| j.id).collect::<Vec<_>>(), vec![low]);
    })
    .await;
}

#[tokio::test]
async fn skips_unknown_kinds_future_jobs_and_paused_queues() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        add(&pool, "q", "other", 0, -1.0, None).await;
        add(&pool, "q", "k", 0, 60.0, None).await;
        add(&pool, "p", "k", 0, -1.0, None).await;
        sqlx::query("INSERT INTO job_queue_pauses (queue, paused_by) VALUES ('p', 'test')").execute(&pool).await.unwrap();
        let kinds = kinds();
        for queue in ["q", "p"] {
            let req = ClaimRequest { queue, kinds: &kinds, process_id: me, limit: 10, max_running: None };
            assert!(claim(&pool, &req).await.unwrap().is_empty(), "queue {queue}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claimers_never_take_the_same_job() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             SELECT 'k', 'q', '{}', 10, 2, 900, 60 FROM generate_series(1, 2000)",
        ).execute(&pool).await.unwrap();
        let mut tasks = Vec::new();
        for i in 0..8 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let me = register_process(&pool, &format!("p{i}")).await;
                let kinds = kinds();
                let mut mine = Vec::new();
                loop {
                    let req = ClaimRequest { queue: "q", kinds: &kinds, process_id: me, limit: 25, max_running: None };
                    let got = claim(&pool, &req).await.unwrap();
                    if got.is_empty() { break; }
                    mine.extend(got.into_iter().map(|j| j.id));
                }
                mine
            }));
        }
        let mut all = Vec::new();
        for t in tasks { all.extend(t.await.unwrap()); }
        let unique: HashSet<i64> = all.iter().copied().collect();
        assert_eq!(all.len(), 2000);
        assert_eq!(unique.len(), 2000, "a job was claimed twice");
    })
    .await;
}

/// Claim in a loop from `claimers` tasks, finishing each job after `hold`,
/// while a sampler checks `bad_sql` (a count that must stay 0) every 5 ms.
async fn stress(pool: PgPool, queue: &'static str, max_running: Option<i64>, bad_sql: &'static str) {
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (pool, stop) = (pool.clone(), stop.clone());
        tokio::spawn(async move {
            let mut worst = 0;
            while !stop.load(Ordering::Relaxed) {
                worst = worst.max(count(&pool, bad_sql).await);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            worst
        })
    };
    let mut tasks = Vec::new();
    for i in 0..6 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let me = register_process(&pool, &format!("{queue}{i}")).await;
            let kinds = kinds();
            let mut idle = 0;
            while idle < 20 {
                let req = ClaimRequest { queue, kinds: &kinds, process_id: me, limit: 5, max_running };
                let got = claim(&pool, &req).await.unwrap();
                if got.is_empty() { idle += 1; tokio::time::sleep(Duration::from_millis(10)).await; continue; }
                idle = 0;
                tokio::time::sleep(Duration::from_millis(20)).await;
                for j in got {
                    sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = now(), locked_by = NULL, locked_until = NULL WHERE id = $1")
                        .bind(j.id).execute(&pool).await.unwrap();
                }
            }
        }));
    }
    for t in tasks { t.await.unwrap(); }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(sampler.await.unwrap(), 0, "limit exceeded: {bad_sql}");
    assert_eq!(count(&pool, "SELECT count(*) FROM jobs WHERE state <> 'succeeded'").await, 0, "jobs left over");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn queue_max_running_is_never_exceeded() {
    with_db(|pool| async move {
        for _ in 0..40 { add(&pool, "imports", "k", 0, -1.0, None).await; }
        stress(pool, "imports", Some(2),
            "SELECT greatest(count(*) - 2, 0) FROM jobs WHERE queue = 'imports' AND state = 'running'").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn at_most_one_running_job_per_concurrency_key() {
    with_db(|pool| async move {
        for i in 0..30 { add(&pool, "keyed", "k", 0, -1.0, Some(if i % 2 == 0 { "a" } else { "b" })).await; }
        stress(pool, "keyed", None,
            "SELECT count(*) FROM (SELECT concurrency_key FROM jobs WHERE state = 'running'
                                   GROUP BY concurrency_key HAVING count(*) > 1) x").await;
    })
    .await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test claim`
Expected: compile error (`store::claim` not found).

- [ ] **Step 3: Implement `store/claim.rs`**

```rust
//! Claiming (spec §7, §12): one short transaction; the handler runs after it
//! commits, so a running job holds no lock and no connection.

use serde_json::Value;
use sqlx::{PgPool, Postgres, Transaction};

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
}

const CLAIM_UNKEYED: &str = "
UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $3,
       locked_until = now() + make_interval(secs => lease_s), attempt_started_at = now()
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
SELECT id FROM jobs j
WHERE state = 'available' AND queue = $1 AND run_at <= now() AND kind = ANY($2)
  AND concurrency_key IS NOT NULL AND NOT (concurrency_key = ANY($3))
  AND NOT EXISTS (SELECT 1 FROM jobs r WHERE r.state = 'running' AND r.concurrency_key = j.concurrency_key)
ORDER BY priority, run_at, id
LIMIT 1
FOR UPDATE SKIP LOCKED";

const CLAIM_ONE: &str = "
UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $2,
       locked_until = now() + make_interval(secs => lease_s), attempt_started_at = now()
WHERE id = $1
RETURNING id, kind, queue, args, attempt, max_attempts, deadline_s, lease_s,
          extract(epoch FROM now() - run_at)::float8 AS waited_s, concurrency_key";

pub async fn claim(pool: &PgPool, req: &ClaimRequest<'_>) -> Result<Vec<ClaimedJob>, sqlx::Error> {
    if req.limit <= 0 || req.kinds.is_empty() {
        return Ok(Vec::new());
    }
    let mut tx = pool.begin().await?;
    let paused: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM job_queue_pauses WHERE queue = $1)")
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
        let running: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE state = 'running' AND queue = $1")
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
        .fetch_all(&mut *tx)
        .await?;
    claim_keyed(&mut tx, req, limit, &mut claimed).await?;
    tx.commit().await?;
    Ok(claimed)
}

/// Jobs with a concurrency key, one per statement: the unique index
/// `jobs_one_running_per_key` rejects a second running job with the same key;
/// that rejection means another worker won, so we skip the key for this round.
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
        let Some(id): Option<i64> = sqlx::query_scalar(PICK_KEYED)
            .bind(req.queue)
            .bind(req.kinds)
            .bind(&skip)
            .fetch_optional(&mut **tx)
            .await?
        else {
            return Ok(());
        };
        let mut savepoint = tx.begin().await?;
        let row: Result<(ClaimedJob, Option<String>), sqlx::Error> = sqlx::query_as::<_, KeyedRow>(CLAIM_ONE)
            .bind(id)
            .bind(req.process_id)
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
            Err(err) if err.as_database_error().is_some_and(|e| e.is_unique_violation()) => {
                savepoint.rollback().await?;
                let key: Option<String> = sqlx::query_scalar("SELECT concurrency_key FROM jobs WHERE id = $1")
                    .bind(id)
                    .fetch_one(&mut **tx)
                    .await?;
                skip.extend(key);
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
```

Add `pub mod claim;` to `store/mod.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-jobs --test claim`
Expected: 5 PASS. If the keyed stress test is flaky, the bug is in `claim_keyed` (not in the test): every claim must go through the unique index.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: SKIP LOCKED claiming with pauses, global queue limits and per-key concurrency"
```

---

### Task 6: Fenced outcome writes

**Files:**
- Create: `crates/jobs/src/store/finish.rs`
- Modify: `crates/jobs/src/store/mod.rs`
- Test: `crates/jobs/tests/finish.rs`

**Interfaces:**
- Consumes: `Backoff` (Task 2).
- Produces: `store::finish::Fence { job_id: i64, process_id: i64, attempt: i32 }`; `enum Cause { Error, Panic, DeadlineExceeded, Permanent, BadArgs, Cancelled, ReleasedOnShutdown }` with `as_str()` (`"error"`, `"panic"`, `"deadline_exceeded"`, `"permanent"`, `"bad_args"`, `"cancelled"`, `"released_on_shutdown"`); `enum Next { RetryIn(Duration), Fail }`; `next_after_failure(attempt, max_attempts, retryable: bool, min_delay: Option<Duration>, &Backoff) -> Next`; `succeed(&PgPool, Fence) -> Result<bool, sqlx::Error>`, `fail_attempt(&PgPool, Fence, Cause, &str, Next) -> Result<bool>`, `cancel(&PgPool, Fence, &str) -> Result<bool>`, `release(&PgPool, Fence) -> Result<bool>`, `write_progress(&PgPool, Fence, &Value) -> Result<bool>`. Each returns `false` when the fence rejected the write. `pub const MAX_ERROR_CHARS: usize = 2000`.
- Produces (tests): `common::force_running(&PgPool, job_id, process_id) -> i32` (claims a job by hand, returns the new attempt).

- [ ] **Step 1: Write the failing tests**

Append to `crates/jobs/tests/common/mod.rs`:

```rust
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
```

Create `crates/jobs/tests/finish.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::{force_running, new_job, register_process, with_db};
use library_jobs::store::finish::{Cause, Fence, MAX_ERROR_CHARS, Next, cancel, fail_attempt, release, succeed, write_progress};
use sqlx::PgPool;

async fn state(pool: &PgPool, id: i64) -> (String, i32, bool, bool) {
    sqlx::query_as("SELECT state, attempt, locked_by IS NULL, finished_at IS NOT NULL FROM jobs WHERE id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn succeed_needs_the_exact_claim_identity() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        assert!(!succeed(&pool, Fence { job_id: id, process_id: me + 1, attempt }).await.unwrap());
        assert!(!succeed(&pool, Fence { job_id: id, process_id: me, attempt: attempt + 1 }).await.unwrap());
        assert_eq!(state(&pool, id).await.0, "running");
        assert!(succeed(&pool, Fence { job_id: id, process_id: me, attempt }).await.unwrap());
        assert_eq!(state(&pool, id).await, ("succeeded".into(), 1, true, true));
        assert!(!succeed(&pool, Fence { job_id: id, process_id: me, attempt }).await.unwrap(), "only once");
    })
    .await;
}

#[tokio::test]
async fn failed_attempts_retry_later_or_fail_and_record_the_cause() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        let f = Fence { job_id: id, process_id: me, attempt };
        assert!(fail_attempt(&pool, f, Cause::Error, "boom", Next::RetryIn(Duration::from_secs(30))).await.unwrap());
        let (st, later, cause, msg): (String, bool, String, String) = sqlx::query_as(
            "SELECT state, run_at > now() + interval '29 s', errors->0->>'cause', errors->0->>'message' FROM jobs WHERE id = $1",
        ).bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((st.as_str(), later, cause.as_str(), msg.as_str()), ("available", true, "error", "boom"));

        let attempt = force_running(&pool, id, me).await;
        let f = Fence { job_id: id, process_id: me, attempt };
        assert!(fail_attempt(&pool, f, Cause::Permanent, "bad input", Next::Fail).await.unwrap());
        assert_eq!(state(&pool, id).await, ("failed".into(), 2, true, true));
    })
    .await;
}

#[tokio::test]
async fn errors_are_capped_and_truncated() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let huge = "é".repeat(1_000_000);
        for _ in 0..12 {
            let attempt = force_running(&pool, id, me).await;
            fail_attempt(&pool, Fence { job_id: id, process_id: me, attempt }, Cause::Error, &huge, Next::RetryIn(Duration::ZERO))
                .await.unwrap();
        }
        let (n, first_attempt, len): (i32, i32, i32) = sqlx::query_as(
            "SELECT jsonb_array_length(errors), (errors->0->>'attempt')::int, length(errors->0->>'message') FROM jobs WHERE id = $1",
        ).bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 10);
        assert_eq!(first_attempt, 3, "the two oldest entries were dropped");
        assert_eq!(len as usize, MAX_ERROR_CHARS);
    })
    .await;
}

#[tokio::test]
async fn release_gives_the_attempt_back_and_cancel_is_terminal() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let a = new_job(&pool, "k").await;
        let attempt = force_running(&pool, a, me).await;
        assert!(release(&pool, Fence { job_id: a, process_id: me, attempt }).await.unwrap());
        assert_eq!(state(&pool, a).await, ("available".into(), 0, true, false));
        let b = new_job(&pool, "k").await;
        let attempt = force_running(&pool, b, me).await;
        assert!(cancel(&pool, Fence { job_id: b, process_id: me, attempt }, "operator").await.unwrap());
        assert_eq!(state(&pool, b).await, ("cancelled".into(), 1, true, true));
    })
    .await;
}

#[tokio::test]
async fn progress_writes_are_fenced() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        let p = serde_json::json!({ "done": 3, "total": 10 });
        assert!(write_progress(&pool, Fence { job_id: id, process_id: me, attempt }, &p).await.unwrap());
        assert!(!write_progress(&pool, Fence { job_id: id, process_id: me + 1, attempt }, &serde_json::json!({})).await.unwrap());
        let stored: serde_json::Value = sqlx::query_scalar("SELECT progress FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(stored, p);
    })
    .await;
}
```

Add unit tests at the bottom of `store/finish.rs` (Step 3 writes the file; include this block):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_until_the_last_attempt_honouring_min_delay() {
        let b = Backoff { base: Duration::from_secs(1), cap: Duration::from_secs(1) };
        assert!(matches!(next_after_failure(1, 3, true, None, &b), Next::RetryIn(d) if d <= Duration::from_secs(1)));
        assert!(matches!(next_after_failure(3, 3, true, None, &b), Next::Fail));
        assert!(matches!(next_after_failure(1, 3, false, None, &b), Next::Fail));
        assert!(matches!(
            next_after_failure(1, 3, true, Some(Duration::from_secs(120)), &b),
            Next::RetryIn(d) if d == Duration::from_secs(120)
        ));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test finish`
Expected: compile error (`store::finish` not found).

- [ ] **Step 3: Implement `store/finish.rs`**

```rust
//! Outcome writes, all fenced by the claim's identity (spec §8.3, §9.1).
//! A write that updates 0 rows means the lease was lost (reaped, maybe
//! re-claimed elsewhere): the caller drops its result.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;

use crate::backoff::Backoff;

pub const MAX_ERROR_CHARS: usize = 2000;

/// Who holds the job, for which attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    pub job_id: i64,
    pub process_id: i64,
    pub attempt: i32,
}

/// Why an attempt ended without success, as stored in `errors[].cause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Error,
    Panic,
    DeadlineExceeded,
    Permanent,
    BadArgs,
    Cancelled,
    ReleasedOnShutdown,
}

impl Cause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Panic => "panic",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Permanent => "permanent",
            Self::BadArgs => "bad_args",
            Self::Cancelled => "cancelled",
            Self::ReleasedOnShutdown => "released_on_shutdown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    RetryIn(Duration),
    Fail,
}

/// Retry while attempts remain and the error allows it; the delay is the
/// kind's backoff, but never shorter than `min_delay` (a `Retry-After`).
pub fn next_after_failure(
    attempt: i32,
    max_attempts: i32,
    retryable: bool,
    min_delay: Option<Duration>,
    backoff: &Backoff,
) -> Next {
    if !retryable || attempt >= max_attempts {
        return Next::Fail;
    }
    Next::RetryIn(backoff.delay(attempt).max(min_delay.unwrap_or_default()))
}

fn truncate(message: &str) -> String {
    message.chars().take(MAX_ERROR_CHARS).collect()
}

pub async fn succeed(pool: &PgPool, f: Fence) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'succeeded', finished_at = now(), locked_by = NULL, locked_until = NULL
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn fail_attempt(pool: &PgPool, f: Fence, cause: Cause, message: &str, next: Next) -> Result<bool, sqlx::Error> {
    let (retry, delay_s) = match next {
        Next::RetryIn(d) => (true, d.as_secs_f64()),
        Next::Fail => (false, 0.0),
    };
    let r = sqlx::query(
        "UPDATE jobs SET
             state = CASE WHEN $4 THEN 'available' ELSE 'failed' END,
             run_at = CASE WHEN $4 THEN now() + make_interval(secs => $5::float8) ELSE run_at END,
             finished_at = CASE WHEN $4 THEN NULL ELSE now() END,
             locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', $6::text, 'message', $7::text))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(retry)
    .bind(delay_s)
    .bind(cause.as_str())
    .bind(truncate(message))
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn cancel(pool: &PgPool, f: Fence, message: &str) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = now(), locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', 'cancelled', 'message', $4::text))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(truncate(message))
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Shutdown is not the job's fault: give the attempt back (spec §13 step 4).
pub async fn release(pool: &PgPool, f: Fence) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = attempt - 1, run_at = now(),
             locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', 'released_on_shutdown',
                 'message', 'the worker shut down before the job finished'))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn write_progress(pool: &PgPool, f: Fence, progress: &Value) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET progress = $4
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(progress)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}
```

Finish the file with the unit-test module from Step 1.

Add `pub mod finish;` to `store/mod.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-jobs --test finish && cargo test -p library-jobs --lib`
Expected: 5 integration PASS, unit tests PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: fenced outcome writes (succeed, retry/fail with capped errors, cancel, release, progress)"
```

---

### Task 7: Registry and the executor (one job, start to recorded outcome)

**Files:**
- Create: `crates/jobs/src/registry.rs`, `crates/jobs/src/executor.rs`
- Modify: `crates/jobs/src/lib.rs`, `crates/jobs/tests/common/mod.rs` (more test kinds)
- Test: `crates/jobs/tests/execute.rs`

**Interfaces:**
- Consumes: `Job`, `JobOptions`, `JobError`, `JobContext::new`, `Deps` (Task 3); `RawJob` (Task 3); `ClaimedJob` (Task 5); `Fence`, `Cause`, `Next`, `next_after_failure`, writes (Task 6); `WorkerConfig` (Task 2); `RateLimiter`.
- Produces:
  - `Registry::new()`, `.register::<J: Job>() -> Self`, `.recurring::<J: Job>(task: &'static str, every: Duration, make: impl Fn() -> J + Send + Sync + 'static) -> Self`, `.provide<T: Send + Sync + 'static>(Arc<T>) -> Self`, `.kinds_for_queue(&str) -> Vec<String>`, `.kinds() -> Vec<String>`, `.validate(&WorkerConfig) -> Result<(), RegistryError>`; `pub(crate) get(&str) -> Option<&KindEntry>`, `pub(crate) recurring_tasks() -> &[Recurring]`, `pub(crate) deps() -> Arc<Deps>`; `RegistryError(Vec<String>)`.
  - `executor::CancelReason { Requested, LeaseLost, Shutdown }`, `executor::CancelCell` (`Default`, `set(CancelReason)` first wins, `get() -> Option<CancelReason>`), `executor::Execution { job: ClaimedJob, process_id: i64, token: CancellationToken, cancel: Arc<CancelCell>, progress: Arc<Mutex<Option<Value>>> }`, `executor::Finished { Succeeded, Retrying, Failed, Cancelled, Released, LeaseLost, FenceRejected }` with `as_str()`, `executor::execute(&PgPool, &Registry, Arc<RateLimiter>, Execution) -> Finished`.
- Produces (tests): kinds `Fails { mode: String }` (`"retry" | "retry_after" | "permanent" | "panic" | "huge" | "cancelled"`), `Sleepy { ms: u64, honour_cancel: bool }`, `ShortDeadline { ms: u64 }` (deadline 1 s, lease 1 s), `SleepOnce { ms: u64 }` (sleeps only on attempt 1); `common::registry() -> Registry` registering all test kinds; `common::fast_config(queues: &str) -> WorkerConfig`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/jobs/tests/common/mod.rs`:

```rust
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
        library_jobs::JobOptions { max_attempts: 3, lease: StdDuration::from_secs(1), ..Default::default() }
    }
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        match self.mode.as_str() {
            "retry" => Err(JobError::retry("try again")),
            "retry_after" => Err(JobError::RetryAfter(StdDuration::from_secs(120), "slow down".into())),
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
        library_jobs::JobOptions { lease: StdDuration::from_secs(1), ..Default::default() }
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
        library_jobs::JobOptions { lease: StdDuration::from_secs(1), ..Default::default() }
    }
    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        if ctx.attempt() == 1 {
            tokio::time::sleep(StdDuration::from_millis(self.ms)).await;
        }
        sqlx::query("INSERT INTO test_runs (n, attempt) VALUES (0, $1)").bind(ctx.attempt()).execute(ctx.pool()).await?;
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
    let mut c = library_jobs::WorkerConfig::new(library_jobs::QueueConfig::parse_list(queues).unwrap());
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
```

Create `crates/jobs/tests/execute.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fails, Noop, ShortDeadline, Sleepy, register_process, registry, with_db};
use library_jobs::executor::{CancelCell, CancelReason, Execution, Finished, execute};
use library_jobs::store::claim::{ClaimRequest, claim};
use library_jobs::{Enqueuer, Job, RateLimiter};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// Enqueue `job`, claim it as a fresh process, and run it through `execute`.
async fn run<J: Job>(pool: &PgPool, job: J, before: impl FnOnce(&CancellationToken, &CancelCell)) -> (i64, Finished) {
    let mut conn = pool.acquire().await.unwrap();
    let id = Enqueuer::new(false).enqueue(&mut conn, &job).await.unwrap().id();
    run_claimed(pool, id, before).await
}

async fn run_claimed(pool: &PgPool, id: i64, before: impl FnOnce(&CancellationToken, &CancelCell)) -> (i64, Finished) {
    let me = register_process(pool, &format!("p{id}")).await;
    let kinds = registry().kinds();
    let req = ClaimRequest { queue: "default", kinds: &kinds, process_id: me, limit: 1, max_running: None };
    let job = claim(pool, &req).await.unwrap().pop().expect("claimed");
    assert_eq!(job.id, id);
    let token = CancellationToken::new();
    let cancel = Arc::new(CancelCell::default());
    before(&token, &cancel);
    let exec = Execution { job, process_id: me, token, cancel, progress: Arc::new(Mutex::new(None)) };
    (id, execute(pool, &registry(), Arc::new(RateLimiter::new()), exec).await)
}

async fn row(pool: &PgPool, id: i64) -> (String, i32, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, attempt, errors->-1->>'cause', errors->-1->>'message' FROM jobs WHERE id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn success_is_recorded() {
    with_db(|pool| async move {
        let (id, f) = run(&pool, Noop { n: 7 }, |_, _| {}).await;
        assert_eq!(f, Finished::Succeeded);
        assert_eq!(row(&pool, id).await.0, "succeeded");
    }).await;
}

#[tokio::test]
async fn each_error_kind_maps_to_its_state() {
    with_db(|pool| async move {
        for (mode, finished, state, cause) in [
            ("retry", Finished::Retrying, "available", "error"),
            ("retry_after", Finished::Retrying, "available", "error"),
            ("permanent", Finished::Failed, "failed", "permanent"),
            ("panic", Finished::Retrying, "available", "panic"),
            ("cancelled", Finished::Cancelled, "cancelled", "cancelled"),
        ] {
            let (id, f) = run(&pool, Fails { mode: mode.into() }, |_, _| {}).await;
            assert_eq!(f, finished, "{mode}");
            let (st, _, c, _) = row(&pool, id).await;
            assert_eq!((st.as_str(), c.as_deref()), (state, Some(cause)), "{mode}");
        }
        let later: bool = sqlx::query_scalar(
            "SELECT run_at > now() + interval '119 s' FROM jobs WHERE args->>'mode' = 'retry_after'",
        ).fetch_one(&pool).await.unwrap();
        assert!(later, "Retry-After must be honoured");
    }).await;
}

#[tokio::test]
async fn huge_error_message_is_truncated() {
    with_db(|pool| async move {
        let (id, _) = run(&pool, Fails { mode: "huge".into() }, |_, _| {}).await;
        let (_, _, _, msg) = row(&pool, id).await;
        assert_eq!(msg.unwrap().chars().count(), 2000);
    }).await;
}

#[tokio::test]
async fn undecodable_args_fail_permanently() {
    with_db(|pool| async move {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             VALUES ('test_noop.v1', 'default', '{\"wrong\": true}', 10, 2, 900, 60) RETURNING id",
        ).fetch_one(&pool).await.unwrap();
        let (_, f) = run_claimed(&pool, id, |_, _| {}).await;
        assert_eq!(f, Finished::Failed);
        let (st, _, cause, msg) = row(&pool, id).await;
        assert_eq!((st.as_str(), cause.as_deref()), ("failed", Some("bad_args")));
        assert!(msg.unwrap().contains("missing field `n`"));
    }).await;
}

#[tokio::test]
async fn deadline_ends_the_attempt_and_retries() {
    with_db(|pool| async move {
        let start = tokio::time::Instant::now();
        let (id, f) = run(&pool, ShortDeadline { ms: 5_000 }, |_, _| {}).await;
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        assert_eq!(f, Finished::Retrying);
        assert_eq!(row(&pool, id).await.2.as_deref(), Some("deadline_exceeded"));
    }).await;
}

#[tokio::test]
async fn cancel_reasons_decide_the_write() {
    with_db(|pool| async move {
        let (id, f) = run(&pool, Sleepy { ms: 10_000, honour_cancel: true }, |t, c| { c.set(CancelReason::Requested); t.cancel(); }).await;
        assert_eq!((f, row(&pool, id).await.0.as_str()), (Finished::Cancelled, "cancelled"));

        let (id, f) = run(&pool, Sleepy { ms: 10_000, honour_cancel: true }, |t, c| { c.set(CancelReason::Shutdown); t.cancel(); }).await;
        assert_eq!(f, Finished::Released);
        let (st, attempt, cause, _) = row(&pool, id).await;
        assert_eq!((st.as_str(), attempt, cause.as_deref()), ("available", 0, Some("released_on_shutdown")));

        let (id, f) = run(&pool, Sleepy { ms: 10_000, honour_cancel: true }, |t, c| { c.set(CancelReason::LeaseLost); t.cancel(); }).await;
        assert_eq!(f, Finished::LeaseLost);
        assert_eq!(row(&pool, id).await.0, "running", "a lost lease writes nothing; the reaper owns the job now");
    }).await;
}

#[tokio::test]
async fn a_stolen_job_is_not_overwritten() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let id = Enqueuer::new(false).enqueue(&mut conn, &Sleepy { ms: 300, honour_cancel: false }).await.unwrap().id();
        let pool2 = pool.clone();
        let thief = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            sqlx::query("UPDATE jobs SET locked_by = -42 WHERE id = $1").bind(id).execute(&pool2).await.unwrap();
        });
        let (_, f) = run_claimed(&pool, id, |_, _| {}).await;
        thief.await.unwrap();
        assert_eq!(f, Finished::FenceRejected);
        assert_eq!(row(&pool, id).await.0, "running");
    }).await;
}
```

Add `tokio-util = { version = "0.7", features = ["rt"] }` to `[dev-dependencies]` of `crates/jobs/Cargo.toml` only if the test crate cannot name `tokio_util` (it is a normal dependency, so it should resolve).

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test execute`
Expected: compile errors (`Registry`, `executor` not found).

- [ ] **Step 3: Implement `registry.rs`**

```rust
//! Kind name -> decoder + handler, recurring tasks, and handler dependencies.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::WorkerConfig;
use crate::context::{Deps, JobContext};
use crate::enqueue::{EnqueueError, RawJob};
use crate::job::{Job, JobError, JobOptions};

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>;
pub(crate) type HandlerFn = Arc<dyn Fn(Value, JobContext) -> Result<BoxFuture, serde_json::Error> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct KindEntry {
    pub(crate) queue: &'static str,
    pub(crate) options: JobOptions,
    pub(crate) handler: HandlerFn,
}

/// A task enqueued by the leader once per `every` (spec §14).
#[derive(Clone)]
pub(crate) struct Recurring {
    pub(crate) task: &'static str,
    pub(crate) every: Duration,
    pub(crate) kind: &'static str,
    pub(crate) make: Arc<dyn Fn() -> Result<RawJob, EnqueueError> + Send + Sync>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid job registry: {}", .0.join("; "))]
pub struct RegistryError(pub Vec<String>);

#[derive(Clone, Default)]
pub struct Registry {
    kinds: HashMap<&'static str, KindEntry>,
    duplicates: Vec<&'static str>,
    recurring: Vec<Recurring>,
    deps: Deps,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<J: Job>(mut self) -> Self {
        let handler: HandlerFn = Arc::new(|args, ctx| {
            let job: J = serde_json::from_value(args)?;
            Ok(Box::pin(job.perform(ctx)) as BoxFuture)
        });
        let entry = KindEntry { queue: J::QUEUE, options: J::options(), handler };
        if self.kinds.insert(J::KIND, entry).is_some() {
            self.duplicates.push(J::KIND);
        }
        self
    }

    /// Enqueue `make()` once per `every`, aligned to multiples of `every` since
    /// the Unix epoch in database time. Missed ticks are not replayed.
    pub fn recurring<J: Job>(
        mut self,
        task: &'static str,
        every: Duration,
        make: impl Fn() -> J + Send + Sync + 'static,
    ) -> Self {
        self.recurring.push(Recurring {
            task,
            every,
            kind: J::KIND,
            make: Arc::new(move || RawJob::encode(&make())),
        });
        self
    }

    /// Make `value` available to handlers through [`JobContext::require`].
    pub fn provide<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.deps.insert(value);
        self
    }

    pub fn kinds(&self) -> Vec<String> {
        let mut k: Vec<String> = self.kinds.keys().map(|s| (*s).to_owned()).collect();
        k.sort();
        k
    }

    pub fn kinds_for_queue(&self, queue: &str) -> Vec<String> {
        let mut k: Vec<String> = self
            .kinds
            .iter()
            .filter(|(_, e)| e.queue == queue)
            .map(|(k, _)| (*k).to_owned())
            .collect();
        k.sort();
        k
    }

    pub(crate) fn get(&self, kind: &str) -> Option<&KindEntry> {
        self.kinds.get(kind)
    }

    pub(crate) fn recurring_tasks(&self) -> &[Recurring] {
        &self.recurring
    }

    pub(crate) fn deps(&self) -> Arc<Deps> {
        Arc::new(self.deps.clone())
    }

    pub fn validate(&self, config: &WorkerConfig) -> Result<(), RegistryError> {
        let mut errors: Vec<String> = self.duplicates.iter().map(|k| format!("kind {k} registered twice")).collect();
        let mut kinds: Vec<(&&str, &KindEntry)> = self.kinds.iter().collect();
        kinds.sort_by_key(|(k, _)| **k);
        for (kind, entry) in kinds {
            if let Err(e) = entry.options.validate() {
                errors.push(format!("{kind}: {e}"));
            }
            if entry.options.lease < config.heartbeat * 3 {
                errors.push(format!(
                    "{kind}: lease {:?} is shorter than 3 heartbeats ({:?})",
                    entry.options.lease,
                    config.heartbeat * 3
                ));
            }
        }
        for r in &self.recurring {
            if r.every < Duration::from_secs(1) {
                errors.push(format!("recurring task {}: every must be at least 1 s", r.task));
            }
            if !self.kinds.contains_key(r.kind) {
                errors.push(format!("recurring task {}: kind {} is not registered", r.task, r.kind));
            }
        }
        if errors.is_empty() { Ok(()) } else { Err(RegistryError(errors)) }
    }
}
```

Add unit tests at the bottom of `registry.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QueueConfig;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct A;
    impl Job for A {
        const KIND: &'static str = "a.v1";
        const QUEUE: &'static str = "q";
        async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
            Ok(())
        }
    }

    #[test]
    fn validation_reports_duplicates_and_short_leases() {
        let config = WorkerConfig::new(QueueConfig::parse_list("q:1").unwrap_or_default());
        assert!(Registry::new().register::<A>().validate(&config).is_ok());
        let err = Registry::new().register::<A>().register::<A>().validate(&config).unwrap_err();
        assert!(err.0[0].contains("registered twice"));
        let mut slow = config.clone();
        slow.heartbeat = Duration::from_secs(30);
        assert!(Registry::new().register::<A>().validate(&slow).is_err());
    }
}
```

(`#[allow(clippy::unwrap_used)]` on the test module if clippy complains about `unwrap_err`.)

- [ ] **Step 4: Implement `executor.rs`**

```rust
//! Running one claimed job and recording its outcome (spec §8.4, §9.1).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::context::JobContext;
use crate::job::JobError;
use crate::rate_limit::RateLimiter;
use crate::registry::Registry;
use crate::store::claim::ClaimedJob;
use crate::store::finish::{self, Cause, Fence, Next, next_after_failure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelReason {
    /// An operator or the app asked (`cancel_requested`).
    Requested = 1,
    /// The heartbeat could not renew the lease; another worker may run the job.
    LeaseLost = 2,
    /// This process is shutting down.
    Shutdown = 3,
}

/// Why a handler's token fired. The first reason set wins.
#[derive(Debug, Default)]
pub struct CancelCell(AtomicU8);

impl CancelCell {
    pub fn set(&self, reason: CancelReason) {
        let _ = self.0.compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn get(&self) -> Option<CancelReason> {
        match self.0.load(Ordering::Acquire) {
            1 => Some(CancelReason::Requested),
            2 => Some(CancelReason::LeaseLost),
            3 => Some(CancelReason::Shutdown),
            _ => None,
        }
    }
}

pub struct Execution {
    pub job: ClaimedJob,
    pub process_id: i64,
    pub token: CancellationToken,
    pub cancel: Arc<CancelCell>,
    pub progress: Arc<Mutex<Option<Value>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Succeeded,
    Retrying,
    Failed,
    Cancelled,
    Released,
    LeaseLost,
    /// The outcome write changed nothing: the lease was lost meanwhile.
    FenceRejected,
}

impl Finished {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Released => "released",
            Self::LeaseLost => "lease_lost",
            Self::FenceRejected => "fence_rejected",
        }
    }
}

/// What the handler did, before deciding what to write.
enum Ran {
    Ok,
    Err(JobError),
    Panicked(String),
    Deadline,
}

pub async fn execute(pool: &PgPool, registry: &Registry, limiter: Arc<RateLimiter>, exec: Execution) -> Finished {
    let Execution { job, process_id, token, cancel, progress } = exec;
    let fence = Fence { job_id: job.id, process_id, attempt: job.attempt };
    let Some(entry) = registry.get(&job.kind) else {
        // Claims filter by known kinds, so this means a registry bug; never retry it here.
        return write(fence, || finish::fail_attempt(pool, fence, Cause::Permanent, "unknown kind", Next::Fail), Finished::Failed).await;
    };
    let ctx = JobContext::new(
        pool.clone(),
        job.id,
        job.attempt,
        Arc::from(job.kind.as_str()),
        token.clone(),
        progress,
        limiter,
        registry.deps(),
    );
    let future = match (entry.handler)(job.args.clone(), ctx) {
        Ok(f) => f,
        Err(err) => {
            let msg = format!("arguments do not decode as {}: {err}", job.kind);
            tracing::error!(job_id = job.id, kind = %job.kind, error = %msg, "jobs.bad_args");
            return write(fence, || finish::fail_attempt(pool, fence, Cause::BadArgs, &msg, Next::Fail), Finished::Failed).await;
        }
    };
    let deadline = Duration::from_secs(u64::try_from(job.deadline_s).unwrap_or(1));
    // Own task: a panic becomes a JoinError instead of unwinding through the worker.
    let mut handle = tokio::spawn(future);
    let ran = tokio::select! {
        joined = &mut handle => match joined {
            Ok(Ok(())) => Ran::Ok,
            Ok(Err(e)) => Ran::Err(e),
            Err(e) if e.is_panic() => Ran::Panicked(panic_message(e)),
            Err(e) => Ran::Err(JobError::retry(e)),
        },
        () = tokio::time::sleep(deadline) => {
            handle.abort();
            Ran::Deadline
        }
    };

    let backoff = entry.options.backoff;
    let retry = |cause: Cause, msg: String, min: Option<Duration>| {
        let next = next_after_failure(job.attempt, job.max_attempts, true, min, &backoff);
        let finished = if matches!(next, Next::Fail) { Finished::Failed } else { Finished::Retrying };
        (cause, msg, next, finished)
    };

    // A fired token decides first: the handler stopped because we asked it to.
    if token.is_cancelled() && !matches!(ran, Ran::Ok) {
        match cancel.get() {
            Some(CancelReason::Shutdown) => {
                return write(fence, || finish::release(pool, fence), Finished::Released).await;
            }
            Some(CancelReason::LeaseLost) => return Finished::LeaseLost,
            Some(CancelReason::Requested) | None => {}
        }
    }
    let (cause, msg, next, finished) = match ran {
        Ran::Ok => return write(fence, || finish::succeed(pool, fence), Finished::Succeeded).await,
        Ran::Err(JobError::Cancelled) => {
            return write(fence, || finish::cancel(pool, fence, "cancelled"), Finished::Cancelled).await;
        }
        Ran::Err(JobError::Permanent(m)) => (Cause::Permanent, m, Next::Fail, Finished::Failed),
        Ran::Err(JobError::Retry(m)) => retry(Cause::Error, m, None),
        Ran::Err(JobError::RetryAfter(d, m)) => retry(Cause::Error, m, Some(d)),
        Ran::Panicked(m) => retry(Cause::Panic, m, None),
        Ran::Deadline => retry(Cause::DeadlineExceeded, format!("did not finish within {deadline:?}"), None),
    };
    write(fence, || finish::fail_attempt(pool, fence, cause, &msg, next), finished).await
}

/// Run a fenced write, retrying database errors (spec §11.5) until it is
/// accepted (→ `finished`) or rejected by the fence (→ `FenceRejected`).
async fn write<F, Fut>(fence: Fence, op: F, finished: Finished) -> Finished
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool, sqlx::Error>>,
{
    let mut wait = Duration::from_secs(1);
    loop {
        match op().await {
            Ok(true) => return finished,
            Ok(false) => {
                tracing::warn!(job_id = fence.job_id, attempt = fence.attempt, "jobs.fence_rejected");
                return Finished::FenceRejected;
            }
            Err(err) => {
                tracing::warn!(job_id = fence.job_id, error = %err, "jobs: outcome write failed; retrying");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn panic_message(err: tokio::task::JoinError) -> String {
    match err.try_into_panic() {
        Ok(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_owned()),
        Err(_) => "task failed".to_owned(),
    }
}
```

Tokio catches a panic inside a spawned task and reports it as a `JoinError`, which is how panics stay isolated.

Add to `lib.rs`:

```rust
pub mod executor;
pub mod registry;

pub use registry::{Registry, RegistryError};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p library-jobs --test execute && cargo test -p library-jobs --lib`
Expected: 7 integration PASS; unit tests PASS.

- [ ] **Step 6: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: registry and executor (deadline, panic isolation, bad args, cancel reasons, fenced writes)"
```

---

### Task 8: Running set and the heartbeat task

**Files:**
- Create: `crates/jobs/src/running.rs`, `crates/jobs/src/heartbeat.rs`
- Modify: `crates/jobs/src/lib.rs`

**Interfaces:**
- Consumes: `CancelCell`, `CancelReason` (Task 7); `store::process::{heartbeat, Beat}` (Task 4); `store::finish::{write_progress, Fence}` (Task 6).
- Produces:
  - `running::Running` (Clone): `insert(id, RunningEntry)`, `remove(id)`, `ids() -> Vec<i64>`, `cancel(id, CancelReason)`, `cancel_all(CancelReason)`, `len()`, `take_progress(process_id) -> Vec<(Fence, Value)>`; `RunningEntry { attempt: i32, token: CancellationToken, cancel: Arc<CancelCell>, progress: Arc<Mutex<Option<Value>>> }`.
  - `heartbeat::Health` (`Default`): `beat_ok()`, `last_beat_age() -> Option<Duration>`, `set_fenced()`, `is_fenced()`.
  - `heartbeat::run(pool: PgPool, process_id: i64, running: Running, every: Duration, give_up_after: Duration, health: Arc<Health>, stop: CancellationToken) -> HeartbeatExit`; `enum HeartbeatExit { Stopped, SelfFenced }`.

The heartbeat's behaviour against the database is tested end to end in Task 9 (cancel within one beat, lost leases, self-fence). This task adds the unit-testable parts.

- [ ] **Step 1: Write the failing unit tests**

Create `crates/jobs/src/running.rs` with this test module at the bottom (implementation in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(attempt: i32) -> RunningEntry {
        RunningEntry {
            attempt,
            token: CancellationToken::new(),
            cancel: Arc::new(CancelCell::default()),
            progress: Arc::new(Mutex::new(None)),
        }
    }

    #[test]
    fn cancel_sets_the_reason_once_and_fires_the_token() {
        let r = Running::default();
        let e = entry(1);
        let (token, cell) = (e.token.clone(), e.cancel.clone());
        r.insert(5, e);
        r.cancel(5, CancelReason::Requested);
        r.cancel(5, CancelReason::Shutdown);
        assert!(token.is_cancelled());
        assert_eq!(cell.get(), Some(CancelReason::Requested));
        r.cancel(99, CancelReason::Requested); // unknown id: no-op
    }

    #[test]
    fn take_progress_drains_pending_values_with_their_fence() {
        let r = Running::default();
        let e = entry(3);
        *e.progress.lock().unwrap_or_else(PoisonError::into_inner) = Some(serde_json::json!({"done": 1}));
        r.insert(7, e);
        r.insert(8, entry(1));
        let taken = r.take_progress(42);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].0, Fence { job_id: 7, process_id: 42, attempt: 3 });
        assert!(r.take_progress(42).is_empty());
        let mut ids = r.ids();
        ids.sort();
        assert_eq!(ids, vec![7, 8]);
        r.cancel_all(CancelReason::Shutdown);
        r.remove(7);
        assert_eq!(r.len(), 1);
    }
}
```

Create `crates/jobs/src/heartbeat.rs` with:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_tracks_beats_and_fencing() {
        let h = Health::default();
        assert!(h.last_beat_age().is_none());
        h.beat_ok();
        assert!(h.last_beat_age().is_some_and(|a| a < Duration::from_secs(1)));
        assert!(!h.is_fenced());
        h.set_fenced();
        assert!(h.is_fenced());
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --lib`
Expected: compile errors.

- [ ] **Step 3: Implement `running.rs`**

```rust
//! The jobs this process is running, with their cancellation tokens.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::executor::{CancelCell, CancelReason};
use crate::store::finish::Fence;

pub struct RunningEntry {
    pub attempt: i32,
    pub token: CancellationToken,
    pub cancel: Arc<CancelCell>,
    pub progress: Arc<Mutex<Option<Value>>>,
}

#[derive(Clone, Default)]
pub struct Running(Arc<Mutex<HashMap<i64, RunningEntry>>>);

impl Running {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<i64, RunningEntry>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, id: i64, entry: RunningEntry) {
        self.map().insert(id, entry);
    }

    pub fn remove(&self, id: i64) {
        self.map().remove(&id);
    }

    pub fn ids(&self) -> Vec<i64> {
        self.map().keys().copied().collect()
    }

    pub fn len(&self) -> usize {
        self.map().len()
    }

    pub fn is_empty(&self) -> bool {
        self.map().is_empty()
    }

    pub fn cancel(&self, id: i64, reason: CancelReason) {
        if let Some(e) = self.map().get(&id) {
            e.cancel.set(reason);
            e.token.cancel();
        }
    }

    pub fn cancel_all(&self, reason: CancelReason) {
        for e in self.map().values() {
            e.cancel.set(reason);
            e.token.cancel();
        }
    }

    /// Pending progress values, removed from their slots, with the fence to write them under.
    pub fn take_progress(&self, process_id: i64) -> Vec<(Fence, Value)> {
        self.map()
            .iter()
            .filter_map(|(id, e)| {
                let value = e.progress.lock().unwrap_or_else(PoisonError::into_inner).take()?;
                Some((Fence { job_id: *id, process_id, attempt: e.attempt }, value))
            })
            .collect()
    }
}
```

- [ ] **Step 4: Implement `heartbeat.rs`**

```rust
//! The heartbeat task (spec §8.2): its own task and connection, never a
//! handler's, so a busy handler cannot starve it. Also flushes progress (§10).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::executor::CancelReason;
use crate::running::Running;
use crate::store::finish::write_progress;
use crate::store::process::{Beat, heartbeat};

/// Liveness for `/readyz`.
#[derive(Debug, Default)]
pub struct Health {
    last_beat_ms: AtomicU64,
    fenced: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

impl Health {
    pub fn beat_ok(&self) {
        self.last_beat_ms.store(now_ms(), Ordering::Release);
    }

    /// Time since the last successful heartbeat (process clock: only compared with itself).
    pub fn last_beat_age(&self) -> Option<Duration> {
        match self.last_beat_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(now_ms().saturating_sub(ms))),
        }
    }

    pub fn set_fenced(&self) {
        self.fenced.store(true, Ordering::Release);
    }

    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatExit {
    Stopped,
    /// The registry row was deleted: the process must stop (spec §8.2).
    SelfFenced,
}

/// Beat every `every` until `stop`. When beats fail for longer than
/// `give_up_after` (the shortest lease), every handler is cancelled, because
/// another worker may legitimately take its job over.
pub async fn run(
    pool: PgPool,
    process_id: i64,
    running: Running,
    every: Duration,
    give_up_after: Duration,
    health: Arc<Health>,
    stop: CancellationToken,
) -> HeartbeatExit {
    let mut beat = tokio::time::interval(every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_ok = tokio::time::Instant::now();
    loop {
        tokio::select! {
            () = stop.cancelled() => return HeartbeatExit::Stopped,
            _ = flush.tick() => {
                for (fence, value) in running.take_progress(process_id) {
                    if let Err(err) = write_progress(&pool, fence, &value).await {
                        tracing::warn!(job_id = fence.job_id, error = %err, "jobs: progress write failed");
                    }
                }
            }
            _ = beat.tick() => {
                let ids = running.ids();
                match heartbeat(&pool, process_id, &ids).await {
                    Ok(Beat::Pruned) => {
                        tracing::error!(process_id, "process.self_fenced: registry row gone; stopping");
                        health.set_fenced();
                        running.cancel_all(CancelReason::LeaseLost);
                        return HeartbeatExit::SelfFenced;
                    }
                    Ok(Beat::Alive(rows)) => {
                        last_ok = tokio::time::Instant::now();
                        health.beat_ok();
                        let renewed: HashSet<i64> = rows.iter().map(|r| r.id).collect();
                        for id in ids.iter().filter(|id| !renewed.contains(id)) {
                            tracing::warn!(job_id = id, "jobs.lease_lost");
                            running.cancel(*id, CancelReason::LeaseLost);
                        }
                        for row in rows.iter().filter(|r| r.cancel_requested) {
                            running.cancel(row.id, CancelReason::Requested);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "jobs: heartbeat failed");
                        if last_ok.elapsed() > give_up_after && !running.is_empty() {
                            tracing::error!("jobs: no heartbeat for a whole lease; cancelling all handlers");
                            running.cancel_all(CancelReason::LeaseLost);
                        }
                    }
                }
            }
        }
    }
}
```

Add to `lib.rs`:

```rust
pub mod heartbeat;
pub mod running;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p library-jobs --lib`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: running set and heartbeat task (lease renewal, lost leases, cancel requests, self-fence, progress flush)"
```

---
### Task 9: Metrics and the Worker (queue loops, LISTEN, graceful shutdown)

**Files:**
- Create: `crates/jobs/src/metrics.rs`, `crates/jobs/src/worker.rs`
- Modify: `crates/jobs/src/registry.rs` (add `min_lease`), `crates/jobs/src/lib.rs`, `crates/jobs/tests/common/mod.rs`
- Test: `crates/jobs/tests/worker.rs`

**Interfaces:**
- Consumes: everything from Tasks 2–8.
- Produces:
  - `Metrics::new()`, `encode() -> String`; crate-internal recorders `claimed(queue, n)`, `waited(kind, secs)`, `finished(kind, Finished, Duration)`, `db_error(op)`, `running_add(queue, i64)`, `set_slots(queue, i64)`, `set_queue_gauges(queue, available, scheduled, running, oldest_s: f64)`, `set_failed_recent(kind, i64)`, `set_processes_live(i64)`, `set_leader(bool)`, `clear_leader_gauges()`. Metric names (counters get `_total` appended on output): `jobs_claimed`, `jobs_completed{kind,outcome}`, `jobs_duration_seconds{kind}`, `jobs_wait_seconds{kind}`, `jobs_fence_rejected`, `jobs_lease_lost`, `jobs_db_errors{op}`, `jobs_running{queue}`, `jobs_slots{queue}`, leader-only `jobs_available{queue}`, `jobs_scheduled{queue}`, `jobs_running_all{queue}`, `jobs_oldest_available_seconds{queue}`, `jobs_failed_recent{kind}`, `jobs_processes_live`, `jobs_leader`.
  - `Worker::new(PgPool, Registry, WorkerConfig) -> Result<Worker, WorkerError>`, `worker.metrics() -> Arc<Metrics>`, `worker.health() -> Arc<Health>`, `worker.run(shutdown: CancellationToken) -> Result<ExitReason, WorkerError>`; `enum ExitReason { Shutdown, SelfFenced }`; `WorkerError::{Config, Registry, Db}`.
  - `Registry::min_lease() -> Option<Duration>` (crate-internal).
- Produces (tests): `common::TestWorker { stop, handle, metrics, health }`, `common::start(&PgPool, WorkerConfig) -> TestWorker`, `TestWorker::shutdown(self) -> ExitReason`, `common::enqueue(&PgPool, &J) -> i64`, `common::job_state(&PgPool, i64) -> String`.

- [ ] **Step 1: Write the failing tests**

Append to `crates/jobs/tests/common/mod.rs`:

```rust
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

pub fn start_with(pool: &sqlx::PgPool, registry: library_jobs::Registry, config: library_jobs::WorkerConfig) -> TestWorker {
    let worker = Worker::new(pool.clone(), registry, config).expect("valid worker");
    let (metrics, health) = (worker.metrics(), worker.health());
    let stop = CancellationToken::new();
    let handle = tokio::spawn(worker.run(stop.clone()));
    TestWorker { stop, handle, metrics, health }
}

impl TestWorker {
    pub async fn shutdown(self) -> ExitReason {
        self.stop.cancel();
        self.handle.await.expect("worker task").expect("worker run")
    }
}

pub async fn enqueue<J: Job>(pool: &sqlx::PgPool, job: &J) -> i64 {
    let mut conn = pool.acquire().await.unwrap();
    library_jobs::Enqueuer::new(true).enqueue(&mut conn, job).await.unwrap().id()
}

pub async fn job_state(pool: &sqlx::PgPool, id: i64) -> String {
    sqlx::query_scalar("SELECT state FROM jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}
```

Create `crates/jobs/tests/worker.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::worker::ExitReason;

const T: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_job_runs_exactly_once_across_four_workers() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             SELECT 'test_noop.v1', 'default', jsonb_build_object('n', i), 10, 2, 900, 1
             FROM generate_series(1, 5000) i",
        ).execute(&pool).await.unwrap();
        let mut config = fast_config("default:8");
        config.maintenance.enabled = false;
        let workers: Vec<TestWorker> = (0..4).map(|_| start(&pool, config.clone())).collect();
        eventually(Duration::from_secs(90), "all 5000 succeeded", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'succeeded'").await == 5000
        }).await;
        for w in workers { assert_eq!(w.shutdown().await, ExitReason::Shutdown); }
        assert_eq!(count(&pool, "SELECT count(*) FROM test_runs").await, 5000);
        assert_eq!(count(&pool, "SELECT count(DISTINCT n) FROM test_runs").await, 5000);
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_wakes_an_idle_worker_long_before_its_poll() {
    with_db(|pool| async move {
        let mut config = fast_config("default:2");
        config.poll = Duration::from_secs(30);
        let w = start(&pool, config);
        tokio::time::sleep(Duration::from_millis(500)).await; // let it LISTEN and go idle
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        eventually(Duration::from_secs(2), "woken by NOTIFY", || async { job_state(&pool, id).await == "succeeded" }).await;
        w.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_listen_the_poll_still_finds_jobs() {
    with_db(|pool| async move {
        let mut config = fast_config("default:2");
        config.listen = false;
        let w = start(&pool, config);
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        eventually(Duration::from_secs(2), "found by polling", || async { job_state(&pool, id).await == "succeeded" }).await;
        w.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_request_stops_the_handler_within_one_heartbeat() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let id = enqueue(&pool, &Sleepy { ms: 30_000, honour_cancel: true }).await;
        eventually(T, "running", || async { job_state(&pool, id).await == "running" }).await;
        sqlx::query("UPDATE jobs SET cancel_requested = true WHERE id = $1").bind(id).execute(&pool).await.unwrap();
        eventually(Duration::from_secs(1), "cancelled", || async { job_state(&pool, id).await == "cancelled" }).await;
        w.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_lease_cancels_the_handler_and_writes_nothing() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let id = enqueue(&pool, &Sleepy { ms: 30_000, honour_cancel: true }).await;
        eventually(T, "running", || async { job_state(&pool, id).await == "running" }).await;
        sqlx::query("UPDATE jobs SET locked_by = -1 WHERE id = $1").bind(id).execute(&pool).await.unwrap();
        eventually(Duration::from_secs(1), "handler saw cancellation", || async {
            count(&pool, "SELECT count(*) FROM test_runs WHERE n = -1").await == 1
        }).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let holder: i64 = sqlx::query_scalar("SELECT locked_by FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((job_state(&pool, id).await.as_str(), holder), ("running", -1));
        w.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_registry_row_makes_the_worker_fence_itself() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        eventually(T, "registered", || async { count(&pool, "SELECT count(*) FROM job_processes").await == 1 }).await;
        sqlx::query("DELETE FROM job_processes").execute(&pool).await.unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(2), w.handle).await.expect("exits").unwrap().unwrap();
        assert_eq!(reason, ExitReason::SelfFenced);
        assert!(w.health.is_fenced());
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_finishes_short_jobs_and_releases_the_rest() {
    with_db(|pool| async move {
        let mut config = fast_config("default:4");
        config.shutdown_timeout = Duration::from_secs(1);
        let w = start(&pool, config);
        let short1 = enqueue(&pool, &Sleepy { ms: 300, honour_cancel: false }).await;
        let short2 = enqueue(&pool, &Sleepy { ms: 300, honour_cancel: false }).await;
        let long = enqueue(&pool, &Sleepy { ms: 30_000, honour_cancel: true }).await;
        eventually(T, "all running", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'running'").await == 3
        }).await;
        assert_eq!(w.shutdown().await, ExitReason::Shutdown);
        assert_eq!(job_state(&pool, short1).await, "succeeded");
        assert_eq!(job_state(&pool, short2).await, "succeeded");
        let (state, attempt, cause): (String, i32, String) = sqlx::query_as(
            "SELECT state, attempt, errors->-1->>'cause' FROM jobs WHERE id = $1",
        ).bind(long).fetch_one(&pool).await.unwrap();
        assert_eq!((state.as_str(), attempt, cause.as_str()), ("available", 0, "released_on_shutdown"));
        assert_eq!(count(&pool, "SELECT count(*) FROM job_processes").await, 0, "deregistered");
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hung_handler_hits_deadline_and_worker_keeps_going() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let hung = enqueue(&pool, &ShortDeadline { ms: 3_000 }).await;
        let mut others = Vec::new();
        for n in 0..20 { others.push(enqueue(&pool, &Noop { n }).await); }
        eventually(Duration::from_secs(5), "others done while one hangs", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE kind = 'test_noop.v1' AND state = 'succeeded'").await == 20
        }).await;
        eventually(Duration::from_secs(3), "deadline recorded", || async {
            let cause: Option<String> = sqlx::query_scalar("SELECT errors->0->>'cause' FROM jobs WHERE id = $1")
                .bind(hung).fetch_one(&pool).await.unwrap();
            cause.as_deref() == Some("deadline_exceeded")
        }).await;
        w.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_claims_only_its_queues_and_records_its_kinds() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("other:2"));
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(job_state(&pool, id).await, "available");
        let knows: bool = sqlx::query_scalar("SELECT config->'kinds' ? 'test_noop.v1' FROM job_processes")
            .fetch_one(&pool).await.unwrap();
        assert!(knows);
        w.shutdown().await;
    }).await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test worker`
Expected: compile errors (`metrics`, `worker` modules not found).

- [ ] **Step 3: Implement `metrics.rs`**

```rust
//! Prometheus metrics (spec §17.2). Per-process counters and histograms, plus
//! queue gauges that only the maintenance leader sets (one aggregate query for
//! the cluster instead of one per process).

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::executor::Finished;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct QueueLabel {
    pub queue: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabel {
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OutcomeLabel {
    pub kind: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OpLabel {
    pub op: String,
}

type HistFamily = Family<KindLabel, Histogram, fn() -> Histogram>;

fn duration_buckets() -> Histogram {
    // 5 ms to about 22 min.
    Histogram::new(exponential_buckets(0.005, 2.0, 18))
}

fn wait_buckets() -> Histogram {
    Histogram::new(exponential_buckets(0.01, 2.0, 18))
}

pub struct Metrics {
    registry: Registry,
    claimed: Family<QueueLabel, Counter>,
    completed: Family<OutcomeLabel, Counter>,
    duration: HistFamily,
    wait: HistFamily,
    fence_rejected: Counter,
    lease_lost: Counter,
    db_errors: Family<OpLabel, Counter>,
    running: Family<QueueLabel, Gauge>,
    slots: Family<QueueLabel, Gauge>,
    available: Family<QueueLabel, Gauge>,
    scheduled: Family<QueueLabel, Gauge>,
    running_all: Family<QueueLabel, Gauge>,
    oldest_available: Family<QueueLabel, Gauge<f64, AtomicU64>>,
    failed_recent: Family<KindLabel, Gauge>,
    processes_live: Gauge,
    leader: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let m = Self {
            claimed: Family::default(),
            completed: Family::default(),
            duration: Family::new_with_constructor(duration_buckets as fn() -> Histogram),
            wait: Family::new_with_constructor(wait_buckets as fn() -> Histogram),
            fence_rejected: Counter::default(),
            lease_lost: Counter::default(),
            db_errors: Family::default(),
            running: Family::default(),
            slots: Family::default(),
            available: Family::default(),
            scheduled: Family::default(),
            running_all: Family::default(),
            oldest_available: Family::default(),
            failed_recent: Family::default(),
            processes_live: Gauge::default(),
            leader: Gauge::default(),
            registry: Registry::default(),
        };
        registry.register("jobs_claimed", "Jobs claimed by this process", m.claimed.clone());
        registry.register("jobs_completed", "Attempts finished by this process, by outcome", m.completed.clone());
        registry.register("jobs_duration_seconds", "Handler run time", m.duration.clone());
        registry.register("jobs_wait_seconds", "Time from run_at to claim", m.wait.clone());
        registry.register("jobs_fence_rejected", "Outcome writes rejected because the lease was lost", m.fence_rejected.clone());
        registry.register("jobs_lease_lost", "Handlers cancelled because their lease was lost", m.lease_lost.clone());
        registry.register("jobs_db_errors", "Database errors in the worker loops, by operation", m.db_errors.clone());
        registry.register("jobs_running", "Handlers running in this process", m.running.clone());
        registry.register("jobs_slots", "Handler slots of this process", m.slots.clone());
        registry.register("jobs_available", "Jobs ready to run (leader only)", m.available.clone());
        registry.register("jobs_scheduled", "Jobs waiting for run_at (leader only)", m.scheduled.clone());
        registry.register("jobs_running_all", "Running jobs across all processes (leader only)", m.running_all.clone());
        registry.register("jobs_oldest_available_seconds", "Age of the oldest ready job: queue latency (leader only)", m.oldest_available.clone());
        registry.register("jobs_failed_recent", "Jobs failed in the last 24 h (leader only)", m.failed_recent.clone());
        registry.register("jobs_processes_live", "Processes with a recent heartbeat (leader only)", m.processes_live.clone());
        registry.register("jobs_leader", "1 while this process is the maintenance leader", m.leader.clone());
        Self { registry, ..m }
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        if let Err(err) = encode(&mut out, &self.registry) {
            tracing::error!(error = ?err, "encoding metrics failed");
        }
        out
    }

    fn q(queue: &str) -> QueueLabel {
        QueueLabel { queue: queue.to_owned() }
    }

    pub(crate) fn claimed(&self, queue: &str, n: u64) {
        self.claimed.get_or_create(&Self::q(queue)).inc_by(n);
    }

    pub(crate) fn waited(&self, kind: &str, secs: f64) {
        self.wait.get_or_create(&KindLabel { kind: kind.to_owned() }).observe(secs.max(0.0));
    }

    pub(crate) fn finished(&self, kind: &str, finished: Finished, took: Duration) {
        let label = OutcomeLabel { kind: kind.to_owned(), outcome: finished.as_str().to_owned() };
        self.completed.get_or_create(&label).inc();
        self.duration.get_or_create(&KindLabel { kind: kind.to_owned() }).observe(took.as_secs_f64());
        match finished {
            Finished::FenceRejected => {
                self.fence_rejected.inc();
            }
            Finished::LeaseLost => {
                self.lease_lost.inc();
            }
            _ => {}
        }
    }

    pub(crate) fn db_error(&self, op: &str) {
        self.db_errors.get_or_create(&OpLabel { op: op.to_owned() }).inc();
    }

    pub(crate) fn running_add(&self, queue: &str, delta: i64) {
        self.running.get_or_create(&Self::q(queue)).inc_by(delta);
    }

    pub(crate) fn set_slots(&self, queue: &str, n: i64) {
        self.slots.get_or_create(&Self::q(queue)).set(n);
    }

    pub(crate) fn set_queue_gauges(&self, queue: &str, available: i64, scheduled: i64, running: i64, oldest_s: f64) {
        let l = Self::q(queue);
        self.available.get_or_create(&l).set(available);
        self.scheduled.get_or_create(&l).set(scheduled);
        self.running_all.get_or_create(&l).set(running);
        self.oldest_available.get_or_create(&l).set(oldest_s);
    }

    pub(crate) fn set_failed_recent(&self, kind: &str, n: i64) {
        self.failed_recent.get_or_create(&KindLabel { kind: kind.to_owned() }).set(n);
    }

    pub(crate) fn set_processes_live(&self, n: i64) {
        self.processes_live.set(n);
    }

    pub(crate) fn set_leader(&self, leader: bool) {
        self.leader.set(i64::from(leader));
    }

    /// Drop the leader-only series, so a former leader stops exporting stale values.
    pub(crate) fn clear_leader_gauges(&self) {
        self.available.clear();
        self.scheduled.clear();
        self.running_all.clear();
        self.oldest_available.clear();
        self.failed_recent.clear();
        self.processes_live.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_gauges_are_exported_with_labels() {
        let m = Metrics::new();
        m.finished("a.v1", Finished::Succeeded, Duration::from_millis(12));
        m.set_queue_gauges("email", 3, 1, 2, 4.5);
        let text = m.encode();
        assert!(text.contains(r#"jobs_completed_total{kind="a.v1",outcome="succeeded"} 1"#), "{text}");
        assert!(text.contains(r#"jobs_available{queue="email"} 3"#), "{text}");
        m.clear_leader_gauges();
        assert!(!m.encode().contains("jobs_available{"));
    }
}
```

- [ ] **Step 4: Add `min_lease` to the registry**

In `registry.rs`, inside `impl Registry`:

```rust
    /// The shortest lease of any registered kind: after this long without a
    /// successful heartbeat, all handlers must stop (spec §8.2).
    pub(crate) fn min_lease(&self) -> Option<Duration> {
        self.kinds.values().map(|e| e.options.lease).min()
    }
```

- [ ] **Step 5: Implement `worker.rs`**

```rust
//! A worker process (spec §11, §13): one claim loop per queue, one LISTEN
//! connection, one heartbeat task, and (Task 10) the maintenance candidate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ConfigError, QueueConfig, WorkerConfig};
use crate::enqueue::NOTIFY_CHANNEL;
use crate::executor::{CancelCell, CancelReason, Execution, execute};
use crate::heartbeat::{self, Health, HeartbeatExit};
use crate::metrics::Metrics;
use crate::rate_limit::RateLimiter;
use crate::registry::{Registry, RegistryError};
use crate::running::{Running, RunningEntry};
use crate::store::claim::{ClaimRequest, ClaimedJob, claim};
use crate::store::process::{ProcessInfo, deregister, register, set_stopping};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown,
    /// The registry row was deleted (presumed dead): exit non-zero and let the
    /// supervisor restart the process (spec §8.2).
    SelfFenced,
}

pub struct Worker {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    limiter: Arc<RateLimiter>,
}

/// What every queue loop and job task shares.
#[derive(Clone)]
struct Shared {
    pool: PgPool,
    registry: Arc<Registry>,
    running: Running,
    tracker: TaskTracker,
    limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    process_id: i64,
    poll: Duration,
}

impl Worker {
    pub fn new(pool: PgPool, registry: Registry, config: WorkerConfig) -> Result<Self, WorkerError> {
        config.validate()?;
        registry.validate(&config)?;
        Ok(Self {
            pool,
            registry,
            config,
            metrics: Arc::new(Metrics::new()),
            health: Arc::new(Health::default()),
            limiter: Arc::new(RateLimiter::new()),
        })
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    pub fn health(&self) -> Arc<Health> {
        self.health.clone()
    }

    /// Run until `shutdown` fires (graceful, spec §13) or the process is fenced.
    pub async fn run(self, shutdown: CancellationToken) -> Result<ExitReason, WorkerError> {
        let Worker { pool, registry, config, metrics, health, limiter } = self;
        let registry = Arc::new(registry);
        let info = process_info(&config, &registry);
        let Some(process_id) = register_until_ok(&pool, &info, &shutdown).await else {
            return Ok(ExitReason::Shutdown);
        };
        tracing::info!(process_id, name = %info.name, "process.started");

        let running = Running::default();
        let hb_stop = CancellationToken::new();
        let give_up_after = registry.min_lease().unwrap_or(config.heartbeat * 3);
        let mut hb = tokio::spawn(heartbeat::run(
            pool.clone(),
            process_id,
            running.clone(),
            config.heartbeat,
            give_up_after,
            health.clone(),
            hb_stop.clone(),
        ));

        let claim_stop = CancellationToken::new();
        let tracker = TaskTracker::new();
        let shared = Shared {
            pool: pool.clone(),
            registry: registry.clone(),
            running: running.clone(),
            tracker: tracker.clone(),
            limiter,
            metrics: metrics.clone(),
            process_id,
            poll: config.poll,
        };
        let wakes: HashMap<String, Arc<Notify>> =
            config.queues.iter().map(|q| (q.name.clone(), Arc::new(Notify::new()))).collect();
        let mut helpers = JoinSet::new();
        if config.listen {
            helpers.spawn(listen(pool.clone(), wakes.clone(), claim_stop.clone()));
        }
        for q in &config.queues {
            let wake = wakes.get(&q.name).cloned().unwrap_or_default();
            helpers.spawn(queue_loop(shared.clone(), q.clone(), wake, claim_stop.clone()));
        }
        let maintenance_stop = CancellationToken::new();
        // Task 10 spawns the maintenance candidate here.

        let reason = tokio::select! {
            () = shutdown.cancelled() => ExitReason::Shutdown,
            exit = &mut hb => match exit {
                Ok(HeartbeatExit::Stopped) => ExitReason::Shutdown,
                // Fenced, or the heartbeat task died: either way it is unsafe to go on.
                Ok(HeartbeatExit::SelfFenced) | Err(_) => ExitReason::SelfFenced,
            },
        };

        claim_stop.cancel();
        maintenance_stop.cancel();
        if reason == ExitReason::Shutdown {
            tracing::info!(process_id, "process.stopping");
            if let Err(err) = set_stopping(&pool, process_id).await {
                tracing::warn!(error = %err, "jobs: could not mark the process as stopping");
            }
        }
        while helpers.join_next().await.is_some() {}
        tracker.close();

        if reason == ExitReason::SelfFenced {
            // Handlers were cancelled with LeaseLost; their writes will be fenced off.
            let _ = tokio::time::timeout(config.cancel_grace, tracker.wait()).await;
            return Ok(ExitReason::SelfFenced);
        }

        if tokio::time::timeout(config.shutdown_timeout, tracker.wait()).await.is_err() {
            running.cancel_all(CancelReason::Shutdown);
            let _ = tokio::time::timeout(config.cancel_grace, tracker.wait()).await;
        }
        hb_stop.cancel();
        let _ = hb.await;
        if !running.is_empty() {
            tracing::warn!(
                count = running.len(),
                "jobs: handlers ignored cancellation; their leases will expire and the reaper will retry them"
            );
        }
        if let Err(err) = deregister(&pool, process_id).await {
            tracing::warn!(error = %err, "jobs: could not delete the registry row; the pruner will");
        }
        tracing::info!(process_id, "process.stopped");
        Ok(ExitReason::Shutdown)
    }
}

fn process_info(config: &WorkerConfig, registry: &Registry) -> ProcessInfo {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| std::fs::read_to_string("/proc/sys/kernel/hostname").ok().map(|s| s.trim().to_owned()))
        .unwrap_or_else(|| "unknown".to_owned());
    let pid = std::process::id();
    let queues: Vec<_> = config
        .queues
        .iter()
        .map(|q| json!({ "name": q.name, "slots": q.slots, "max_running": q.max_running }))
        .collect();
    ProcessInfo {
        name: format!("{}-{hostname}-{pid}-{:08x}", config.process_kind, rand::random::<u32>()),
        kind: config.process_kind.clone(),
        hostname,
        pid: i32::try_from(pid).unwrap_or(i32::MAX),
        version: config.version.clone(),
        config: json!({ "queues": queues, "kinds": registry.kinds() }),
    }
}

async fn register_until_ok(pool: &PgPool, info: &ProcessInfo, shutdown: &CancellationToken) -> Option<i64> {
    let mut wait = Duration::from_secs(1);
    loop {
        match register(pool, info).await {
            Ok(id) => return Some(id),
            Err(err) => tracing::warn!(error = %err, "jobs: could not register the process; retrying"),
        }
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

fn jittered(d: Duration) -> Duration {
    d.mul_f64(rand::random_range(0.9..=1.1))
}

async fn queue_loop(s: Shared, q: QueueConfig, wake: Arc<Notify>, stop: CancellationToken) {
    let kinds = s.registry.kinds_for_queue(&q.name);
    if kinds.is_empty() {
        tracing::warn!(queue = %q.name, "jobs: no registered kind uses this queue; not claiming from it");
        return;
    }
    let slots = Arc::new(Semaphore::new(q.slots));
    s.metrics.set_slots(&q.name, i64::try_from(q.slots).unwrap_or(i64::MAX));
    let mut err_wait = Duration::from_secs(1);
    loop {
        if stop.is_cancelled() {
            return;
        }
        let free = slots.available_permits();
        let mut full = false;
        if free > 0 {
            let req = ClaimRequest {
                queue: &q.name,
                kinds: &kinds,
                process_id: s.process_id,
                limit: i64::try_from(free).unwrap_or(i64::MAX),
                max_running: q.max_running,
            };
            match claim(&s.pool, &req).await {
                Ok(jobs) => {
                    err_wait = Duration::from_secs(1);
                    full = jobs.len() == free;
                    s.metrics.claimed(&q.name, jobs.len() as u64);
                    for job in jobs {
                        // Only this loop takes permits, and it claimed at most `free`
                        // jobs, so a permit is always there.
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            tracing::error!(job_id = job.id, "jobs: no free slot for a claimed job; its lease will expire");
                            continue;
                        };
                        spawn_job(&s, &q.name, job, permit, wake.clone());
                    }
                }
                Err(err) => {
                    s.metrics.db_error("claim");
                    tracing::warn!(queue = %q.name, error = %err, "jobs: claim failed");
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = tokio::time::sleep(err_wait) => {}
                    }
                    err_wait = (err_wait * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        if full && slots.available_permits() > 0 {
            continue; // more jobs are probably waiting
        }
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep(jittered(s.poll)) => {}
            () = stop.cancelled() => return,
        }
    }
}

fn spawn_job(s: &Shared, queue: &str, job: ClaimedJob, permit: OwnedSemaphorePermit, wake: Arc<Notify>) {
    let token = CancellationToken::new();
    let cancel = Arc::new(CancelCell::default());
    let progress = Arc::new(Mutex::new(None));
    s.running.insert(
        job.id,
        RunningEntry { attempt: job.attempt, token: token.clone(), cancel: cancel.clone(), progress: progress.clone() },
    );
    s.metrics.running_add(queue, 1);
    let s = s.clone();
    let queue = queue.to_owned();
    let tracker = s.tracker.clone();
    tracker.spawn(async move {
        let (id, kind) = (job.id, job.kind.clone());
        tracing::debug!(job_id = id, %kind, attempt = job.attempt, "jobs.claimed");
        s.metrics.waited(&kind, job.waited_s);
        let started = tokio::time::Instant::now();
        let exec = Execution { job, process_id: s.process_id, token, cancel, progress };
        let finished = execute(&s.pool, &s.registry, s.limiter.clone(), exec).await;
        tracing::debug!(job_id = id, %kind, outcome = finished.as_str(), "jobs.finished");
        s.metrics.finished(&kind, finished, started.elapsed());
        s.metrics.running_add(&queue, -1);
        s.running.remove(id);
        drop(permit);
        wake.notify_one();
    });
}

/// LISTEN for enqueue notifications and wake the matching queue loop. Lost
/// notifications only cost latency: every loop also polls.
async fn listen(pool: PgPool, wakes: HashMap<String, Arc<Notify>>, stop: CancellationToken) {
    let mut wait = Duration::from_secs(1);
    loop {
        let connect = async {
            let mut l = PgListener::connect_with(&pool).await?;
            l.listen(NOTIFY_CHANNEL).await?;
            Ok::<_, sqlx::Error>(l)
        };
        let mut listener = tokio::select! {
            () = stop.cancelled() => return,
            r = connect => match r {
                Ok(l) => l,
                Err(err) => {
                    tracing::warn!(error = %err, "jobs: LISTEN failed; polling meanwhile");
                    tokio::select! { () = stop.cancelled() => return, () = tokio::time::sleep(wait) => {} }
                    wait = (wait * 2).min(Duration::from_secs(30));
                    continue;
                }
            },
        };
        wait = Duration::from_secs(1);
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                note = listener.try_recv() => match note {
                    Ok(Some(n)) => {
                        if let Some(w) = wakes.get(n.payload()) {
                            w.notify_one();
                        }
                    }
                    // The connection dropped (sqlx reconnects on the next call);
                    // notifications may have been missed, so wake everyone once.
                    Ok(None) => wakes.values().for_each(|w| w.notify_one()),
                    Err(err) => {
                        tracing::warn!(error = %err, "jobs: LISTEN connection failed");
                        break;
                    }
                },
            }
        }
    }
}
```

Add to `lib.rs`:

```rust
pub mod metrics;
pub mod worker;

pub use worker::{ExitReason, Worker, WorkerError};
```

- [ ] **Step 6: Run the tests**

Run: `cargo test -p library-jobs --test worker && cargo test -p library-jobs --lib`
Expected: 9 + unit PASS. The 5,000-job test takes a few seconds.

- [ ] **Step 7: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: worker with per-queue claim loops, LISTEN wake-ups, metrics, graceful shutdown and self-fencing"
```

---

### Task 10: Maintenance leader, reaper, pruner and leader gauges

**Files:**
- Create: `crates/jobs/src/store/maintenance.rs`, `crates/jobs/src/store/stats.rs`, `crates/jobs/src/maintenance.rs`
- Modify: `crates/jobs/src/store/mod.rs`, `crates/jobs/src/worker.rs` (spawn the candidate), `crates/jobs/src/lib.rs`
- Test: `crates/jobs/tests/maintenance.rs`

**Interfaces:**
- Produces: `store::maintenance::reap(&PgPool) -> Result<Vec<Reaped>, sqlx::Error>` with `Reaped { id: i64, state: String, cause: String }`; `store::maintenance::prune(&PgPool, after: Duration) -> Result<(i64, i64), sqlx::Error>` (processes, jobs); `store::stats::QueueStats { queue, available, scheduled, running, oldest_available_s }`, `queue_stats(&PgPool)`, `failed_recent(&PgPool) -> Vec<(String, i64)>`, `live_processes(&PgPool, within: Duration) -> i64`; `maintenance::LEADER_LOCK: i64`; `pub(crate) maintenance::run(pool, Arc<Registry>, MaintenanceConfig, Enqueuer, Arc<Metrics>, stop)`.
- Consumes: `Enqueuer` (recurring ticks are added in Task 11 inside the same loop).

- [ ] **Step 1: Write the failing tests**

Create `crates/jobs/tests/maintenance.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::maintenance::LEADER_LOCK;
use library_jobs::store::maintenance::{prune, reap};
use sqlx::PgPool;

async fn expire(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1").bind(id).execute(pool).await.unwrap();
}

async fn row(pool: &PgPool, id: i64) -> (String, i32, String) {
    sqlx::query_as("SELECT state, crashes, errors->-1->>'cause' FROM jobs WHERE id = $1").bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn reaper_requeues_expired_leases_with_the_right_cause() {
    with_db(|pool| async move {
        let live = register_process(&pool, "live").await;
        let a = insert_running(&pool, live, "k", 60, 900).await;
        let b = insert_running(&pool, -7, "k", 60, 900).await; // holder has no registry row
        expire(&pool, a).await;
        expire(&pool, b).await;
        let reaped = reap(&pool).await.unwrap();
        assert_eq!(reaped.len(), 2);
        assert_eq!(row(&pool, a).await, ("available".into(), 1, "lease_expired".into()));
        assert_eq!(row(&pool, b).await, ("available".into(), 1, "process_lost".into()));
        assert!(reap(&pool).await.unwrap().is_empty(), "nothing left to reap");
    }).await;
}

#[tokio::test]
async fn poison_jobs_fail_after_max_crashes_and_attempts_are_respected() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = insert_running(&pool, me, "k", 60, 900).await; // max_crashes 2
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, id).await.0, "available");
        force_running(&pool, id, me).await;
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, id).await, ("failed".into(), 2, "lease_expired".into()));

        let last = insert_running(&pool, me, "k", 60, 900).await;
        sqlx::query("UPDATE jobs SET max_attempts = 1, max_crashes = 5 WHERE id = $1").bind(last).execute(&pool).await.unwrap();
        expire(&pool, last).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, last).await.0, "failed", "no attempts left");
    }).await;
}

#[tokio::test]
async fn pruner_removes_silent_processes_and_frees_their_jobs() {
    with_db(|pool| async move {
        let dead = register_process(&pool, "dead").await;
        let alive = register_process(&pool, "alive").await;
        let j = insert_running(&pool, dead, "k", 60, 900).await;
        let keep = insert_running(&pool, alive, "k", 60, 900).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour' WHERE id = $1").bind(dead).execute(&pool).await.unwrap();
        assert_eq!(prune(&pool, Duration::from_secs(180)).await.unwrap(), (1, 1));
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, j).await, ("available".into(), 1, "process_lost".into()));
        assert_eq!(job_state(&pool, keep).await, "running");
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_process_job_is_recovered_by_a_live_worker() {
    with_db(|pool| async move {
        // What `kill -9` leaves behind: a registry row that stops beating and a running job.
        let dead = register_process(&pool, "dead").await;
        let id = enqueue(&pool, &SleepOnce { ms: 60_000 }).await;
        force_running(&pool, id, dead).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour'").execute(&pool).await.unwrap();
        let w = start(&pool, fast_config("default:2"));
        eventually(Duration::from_secs(10), "re-run and succeeded", || async { job_state(&pool, id).await == "succeeded" }).await;
        let (crashes, cause): (i32, String) = sqlx::query_as("SELECT crashes, errors->0->>'cause' FROM jobs WHERE id = $1")
            .bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((crashes, cause.as_str()), (1, "process_lost"));
        w.shutdown().await;
    }).await;
}

async fn leaders(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND ((classid::bigint << 32) | objid::bigint) = $1",
    ).bind(LEADER_LOCK).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_leader_and_failover_when_its_connection_dies() {
    with_db(|pool| async move {
        let a = start(&pool, fast_config("default:1"));
        let b = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "a leader", || async { leaders(&pool).await == 1 }).await;
        for _ in 0..20 {
            assert!(leaders(&pool).await <= 1);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
               AND ((classid::bigint << 32) | objid::bigint) = $1",
        ).bind(LEADER_LOCK).execute(&pool).await.unwrap();
        // A lease that expires now must still be reaped by the new leader.
        let me = register_process(&pool, "victim").await;
        let id = insert_running(&pool, me, "k", 60, 900).await;
        expire(&pool, id).await;
        eventually(Duration::from_secs(5), "reaped after failover", || async { job_state(&pool, id).await == "available" }).await;
        assert_eq!(leaders(&pool).await, 1);
        a.shutdown().await;
        b.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_leader_exports_queue_gauges() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let opts = library_jobs::EnqueueOptions { delay: Some(Duration::from_secs(3600)), ..Default::default() };
        for n in 0..2 {
            library_jobs::Enqueuer::new(false).enqueue_with(&mut conn, &Noop { n }, opts.clone()).await.unwrap();
        }
        let w = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "gauges", || async {
            w.metrics.encode().contains(r#"jobs_scheduled{queue="default"} 2"#)
        }).await;
        assert!(w.metrics.encode().contains("jobs_leader 1"));
        w.shutdown().await;
    }).await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test maintenance`
Expected: compile errors.

- [ ] **Step 3: Implement `store/maintenance.rs` (reap, prune)**

```rust
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
```

- [ ] **Step 4: Implement `store/stats.rs`**

```rust
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
```

Add `pub mod maintenance;` and `pub mod stats;` to `store/mod.rs`.

- [ ] **Step 5: Implement `maintenance.rs` (leader loop)**

```rust
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
    pub(crate) registry: Arc<Registry>,
    pub(crate) config: MaintenanceConfig,
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
    let (mut check, mut reap, mut prune) = (every(c.leader_retry), every(c.reap_every), every(c.prune_every));
    let (mut recurring, mut retention, mut gauges) = (every(c.recurring_every), every(c.retention_every), every(c.gauges_every));
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
                l.metrics.set_queue_gauges(&q.queue, q.available, q.scheduled, q.running, q.oldest_available_s);
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
```

(The two empty functions are filled in Task 11; they are real code paths that do nothing yet, so the loop compiles and its other ticks are testable now.)

Add `pub mod maintenance;` to `lib.rs`.

In `worker.rs`, replace the line `// Task 10 spawns the maintenance candidate here.` with:

```rust
        if config.maintenance.enabled {
            let leader = crate::maintenance::Leader {
                pool: pool.clone(),
                registry: registry.clone(),
                config: config.maintenance.clone(),
                enqueuer: crate::enqueue::Enqueuer::new(config.listen),
                metrics: metrics.clone(),
                live_within: config.heartbeat * 3,
            };
            helpers.spawn(crate::maintenance::run(leader, maintenance_stop.clone()));
        }
```

and move `let maintenance_stop = CancellationToken::new();` above it (it already is).

- [ ] **Step 6: Run the tests**

Run: `cargo test -p library-jobs --test maintenance && cargo test -p library-jobs --test worker`
Expected: 6 PASS; worker tests still PASS.

- [ ] **Step 7: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: advisory-lock maintenance leader with reaper (crash causes, poison limit), pruner and queue gauges"
```

---

### Task 11: Recurring jobs and retention

**Files:**
- Modify: `crates/jobs/src/store/maintenance.rs`, `crates/jobs/src/maintenance.rs`
- Test: `crates/jobs/tests/recurring.rs`

**Interfaces:**
- Consumes: `Registry::recurring_tasks()`, `Recurring { task, every, kind, make }`, `Enqueuer::insert_raw`, `RawJob` (Tasks 3, 7).
- Produces: `store::maintenance::delete_finished(&PgPool, keep_succeeded: Duration, keep_failed: Duration, batch: i64) -> Result<u64, sqlx::Error>`; `pub(crate) store::maintenance::enqueue_tick(&PgPool, &Enqueuer, task: &str, every: Duration, raw: &RawJob) -> Result<Option<i64>, EnqueueError>`.

- [ ] **Step 1: Write the failing tests**

Create `crates/jobs/tests/recurring.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::maintenance::LEADER_LOCK;
use library_jobs::store::maintenance::delete_finished;

#[tokio::test(flavor = "multi_thread")]
async fn each_tick_is_enqueued_once_across_leader_handovers() {
    with_db(|pool| async move {
        let reg = || registry().recurring::<Noop>("tick", Duration::from_secs(1), || Noop { n: 0 });
        let ws: Vec<TestWorker> = (0..3).map(|_| start_with(&pool, reg(), fast_config("default:2"))).collect();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            // Force a hand-over by killing the leader's lock connection.
            sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
                   AND ((classid::bigint << 32) | objid::bigint) = $1",
            ).bind(LEADER_LOCK).execute(&pool).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        for w in ws { w.shutdown().await; }
        let runs = count(&pool, "SELECT count(*) FROM job_recurring_runs WHERE task = 'tick'").await;
        let jobs = count(&pool, "SELECT count(*) FROM jobs WHERE kind = 'test_noop.v1'").await;
        let distinct = count(&pool, "SELECT count(DISTINCT run_at) FROM job_recurring_runs").await;
        assert_eq!(runs, jobs, "one job per recorded tick");
        assert_eq!(runs, distinct);
        assert!((3..=6).contains(&runs), "about one tick per second over ~4.8 s, got {runs}");
    }).await;
}

#[tokio::test]
async fn retention_deletes_only_rows_past_their_window_in_batches() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, state, max_attempts, max_crashes, deadline_s, lease_s, finished_at)
             SELECT 'k', 'q', '{}', s.state, 10, 2, 900, 60, now() - s.age
             FROM (VALUES ('succeeded', interval '2 days', 2500), ('succeeded', interval '1 minute', 10),
                          ('failed', interval '20 days', 5), ('failed', interval '1 day', 5),
                          ('cancelled', interval '20 days', 3)) AS s(state, age, n),
                  LATERAL generate_series(1, s.n)",
        ).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO job_recurring_runs (task, run_at, created_at) VALUES ('old', now(), now() - interval '8 days'), ('new', now(), now())")
            .execute(&pool).await.unwrap();
        let deleted = delete_finished(&pool, Duration::from_secs(86_400), Duration::from_secs(14 * 86_400), 1000).await.unwrap();
        assert_eq!(deleted, 2500 + 5 + 3);
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 15);
        assert_eq!(count(&pool, "SELECT count(*) FROM job_recurring_runs").await, 1);
    }).await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test recurring`
Expected: compile error (`delete_finished` not found) and, once added, the tick test fails (no ticks enqueued).

- [ ] **Step 3: Implement the SQL**

Append to `store/maintenance.rs`:

```rust
use chrono::{DateTime, Utc};

use crate::enqueue::{EnqueueError, EnqueueOptions, Enqueuer, RawJob};

/// Enqueue the current tick of `task` unless some leader already did (spec §14).
/// The tick is the latest multiple of `every` since the epoch, in database time,
/// so missed ticks are never replayed.
pub(crate) async fn enqueue_tick(
    pool: &PgPool,
    enqueuer: &Enqueuer,
    task: &str,
    every: Duration,
    raw: &RawJob,
) -> Result<Option<i64>, EnqueueError> {
    let mut tx = pool.begin().await?;
    let tick: DateTime<Utc> =
        sqlx::query_scalar("SELECT to_timestamp(floor(extract(epoch FROM now()) / $1::float8) * $1::float8)")
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
    let id = enqueuer.insert_raw(&mut *tx, raw, &EnqueueOptions::default()).await?.id();
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
    for (states, keep) in [(vec!["succeeded"], keep_succeeded), (vec!["failed", "cancelled"], keep_failed)] {
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
```

- [ ] **Step 4: Fill in the leader's two tasks**

In `maintenance.rs`, replace the two empty functions:

```rust
async fn run_recurring(l: &Leader) {
    for r in l.registry.recurring_tasks() {
        let raw = match (r.make)() {
            Ok(raw) => raw,
            Err(err) => {
                tracing::error!(task = r.task, error = %err, "jobs: recurring task cannot be encoded");
                continue;
            }
        };
        match sql::enqueue_tick(&l.pool, &l.enqueuer, r.task, r.every, &raw).await {
            Ok(Some(id)) => tracing::info!(task = r.task, job_id = id, "recurring.enqueued"),
            Ok(None) => {}
            Err(err) => {
                l.metrics.db_error("recurring");
                tracing::warn!(task = r.task, error = %err, "jobs: recurring enqueue failed");
            }
        }
    }
}

async fn run_retention(l: &Leader) {
    match sql::delete_finished(&l.pool, l.config.keep_succeeded, l.config.keep_failed, 1000).await {
        Ok(0) => {}
        Ok(n) => tracing::info!(deleted = n, "jobs: retention"),
        Err(err) => {
            l.metrics.db_error("retention");
            tracing::warn!(error = %err, "jobs: retention failed");
        }
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p library-jobs --test recurring && cargo test -p library-jobs`
Expected: 2 PASS, and the whole crate suite PASS.

- [ ] **Step 6: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: recurring ticks de-duplicated by (task, run_at) and batched retention"
```

---

### Task 12: HTTP endpoints (`/metrics`, `/healthz`, `/readyz`)

**Files:**
- Create: `crates/jobs/src/http.rs`
- Modify: `crates/jobs/src/lib.rs`
- Test: `crates/jobs/tests/http.rs`

**Interfaces:**
- Produces: `http::router(metrics: Arc<Metrics>, health: Arc<Health>, pool: PgPool, heartbeat: Duration) -> axum::Router`; `http::serve(listener: tokio::net::TcpListener, router: Router, stop: CancellationToken) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing test**

Create `crates/jobs/tests/http.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(router: &axum::Router, path: &str) -> (StatusCode, String) {
    let res = router.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_heartbeats_and_metrics_count_jobs() {
    with_db(|pool| async move {
        let config = fast_config("default:2");
        let w = start(&pool, config.clone());
        let router = library_jobs::http::router(w.metrics.clone(), w.health.clone(), pool.clone(), config.heartbeat);
        assert_eq!(get(&router, "/healthz").await.0, StatusCode::OK);
        eventually(Duration::from_secs(3), "ready", || async { get(&router, "/readyz").await.0 == StatusCode::OK }).await;
        for n in 0..3 { enqueue(&pool, &Noop { n }).await; }
        eventually(Duration::from_secs(5), "counted", || async {
            get(&router, "/metrics").await.1.contains(r#"jobs_completed_total{kind="test_noop.v1",outcome="succeeded"} 3"#)
        }).await;
        sqlx::query("DELETE FROM job_processes").execute(&pool).await.unwrap();
        eventually(Duration::from_secs(3), "not ready once fenced", || async {
            let (status, body) = get(&router, "/readyz").await;
            status == StatusCode::SERVICE_UNAVAILABLE && body.contains("self-fenced")
        }).await;
        let _ = w.handle.await;
    }).await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test http`
Expected: compile error (`http` module not found).

- [ ] **Step 3: Implement `http.rs`**

```rust
//! `/metrics`, `/healthz`, `/readyz` for a worker process (spec §17.2-17.3).
//! Bind it to an internal address only.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::heartbeat::Health;
use crate::metrics::Metrics;

#[derive(Clone)]
struct HttpState {
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    pool: PgPool,
    stale_after: Duration,
}

pub fn router(metrics: Arc<Metrics>, health: Arc<Health>, pool: PgPool, heartbeat: Duration) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_text))
        .with_state(HttpState { metrics, health, pool, stale_after: heartbeat * 3 })
}

/// Ready: not fenced, a heartbeat within three periods, and the database answers.
async fn readyz(State(s): State<HttpState>) -> (StatusCode, String) {
    let unavailable = |msg: String| (StatusCode::SERVICE_UNAVAILABLE, msg);
    if s.health.is_fenced() {
        return unavailable("self-fenced: the registry row was deleted\n".into());
    }
    match s.health.last_beat_age() {
        None => return unavailable("no heartbeat yet\n".into()),
        Some(age) if age > s.stale_after => return unavailable(format!("last heartbeat {age:?} ago\n")),
        Some(_) => {}
    }
    let ping = tokio::time::timeout(Duration::from_secs(2), sqlx::query("SELECT 1").execute(&s.pool)).await;
    if !matches!(ping, Ok(Ok(_))) {
        return unavailable("database unreachable\n".into());
    }
    (StatusCode::OK, "ready\n".into())
}

async fn metrics_text(State(s): State<HttpState>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/openmetrics-text; version=1.0.0; charset=utf-8")],
        s.metrics.encode(),
    )
}

pub async fn serve(listener: TcpListener, router: Router, stop: CancellationToken) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
}
```

Add `pub mod http;` to `lib.rs`.

- [ ] **Step 4: Run the test**

Run: `cargo test -p library-jobs --test http`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: /metrics, /healthz and /readyz for worker processes"
```

---

### Task 13: Ops queries (behind `library-web jobs …`)

**Files:**
- Create: `crates/jobs/src/ops.rs`
- Modify: `crates/jobs/src/lib.rs`
- Test: `crates/jobs/tests/ops.rs`

**Interfaces:**
- Consumes: `store::stats::{queue_stats, failed_recent}`.
- Produces (all `pub async fn …(pool: &PgPool, …)`):
  - `status(pool) -> Result<Status, sqlx::Error>`; `Status { queues: Vec<QueueStatus>, processes: Vec<ProcessRow>, orphan_kinds: Vec<String> }`; `QueueStatus { queue, available, scheduled, running, failed_24h, oldest_available_s, paused: bool }`
  - `list(pool, &ListFilter) -> Vec<JobSummary>`; `ListFilter { state: Option<String>, queue: Option<String>, kind: Option<String>, limit: i64 }`; `JobSummary { id, kind, queue, state, attempt, max_attempts, crashes, run_at: DateTime<Utc>, created_at, finished_at: Option<DateTime<Utc>>, last_error: Option<String> }`
  - `show(pool, id) -> Option<JobDetail>`; `JobDetail { summary: JobSummary, args: Value, progress: Option<Value>, errors: Value, locked_by: Option<i64>, locked_until: Option<DateTime<Utc>>, unique_key: Option<String>, concurrency_key: Option<String>, cancel_requested: bool }`
  - `retry(pool, id) -> bool`; `retry_failed(pool, kind: Option<&str>, since: Option<Duration>) -> u64`
  - `discard(pool, id) -> bool`
  - `cancel(pool, id) -> CancelOutcome { Cancelled, Requested, AlreadyFinished, NotFound }`
  - `pause(pool, queue, by, reason: Option<&str>)`; `resume(pool, queue) -> bool`
  - `processes(pool) -> Vec<ProcessRow>`; `ProcessRow { id, name, kind, hostname, pid, version, started_at, heartbeat_age_s: f64, stopping: bool }`
  - `prune_preview(pool, keep_succeeded, keep_failed) -> (i64, i64)`

- [ ] **Step 1: Write the failing tests**

Create `crates/jobs/tests/ops.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::ops::{self, CancelOutcome, ListFilter};

#[tokio::test]
async fn status_counts_queues_and_flags_orphan_kinds_and_pauses() {
    with_db(|pool| async move {
        enqueue(&pool, &Noop { n: 1 }).await;
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             VALUES ('ghost.v1', 'default', '{}', 1, 1, 60, 60)",
        ).execute(&pool).await.unwrap();
        let p = register_process(&pool, "w").await;
        sqlx::query("UPDATE job_processes SET config = '{\"queues\": [], \"kinds\": [\"test_noop.v1\"]}' WHERE id = $1")
            .bind(p).execute(&pool).await.unwrap();
        ops::pause(&pool, "default", "tester", Some("maintenance")).await.unwrap();
        let s = ops::status(&pool).await.unwrap();
        let q = &s.queues[0];
        assert_eq!((q.queue.as_str(), q.available, q.paused), ("default", 2, true));
        assert_eq!(s.orphan_kinds, vec!["ghost.v1".to_owned()]);
        assert_eq!(s.processes.len(), 1);
        assert!(ops::resume(&pool, "default").await.unwrap());
        assert!(!ops::status(&pool).await.unwrap().queues[0].paused);
    }).await;
}

#[tokio::test]
async fn list_show_retry_discard_and_cancel() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let waiting = enqueue(&pool, &Noop { n: 1 }).await;
        let running = enqueue(&pool, &Noop { n: 2 }).await;
        force_running(&pool, running, me).await;
        let failed = enqueue(&pool, &Noop { n: 3 }).await;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now(), attempt = 10, crashes = 1,
                     errors = '[{\"cause\": \"error\", \"message\": \"boom\"}]' WHERE id = $1")
            .bind(failed).execute(&pool).await.unwrap();

        let all = ops::list(&pool, &ListFilter { state: None, queue: None, kind: None, limit: 10 }).await.unwrap();
        assert_eq!(all.iter().map(|j| j.id).collect::<Vec<_>>(), vec![failed, running, waiting], "newest first");
        let only_failed = ops::list(&pool, &ListFilter { state: Some("failed".into()), queue: None, kind: None, limit: 10 }).await.unwrap();
        assert_eq!(only_failed[0].last_error.as_deref(), Some("boom"));
        let detail = ops::show(&pool, running).await.unwrap().unwrap();
        assert_eq!(detail.locked_by, Some(me));

        assert!(ops::retry(&pool, failed).await.unwrap());
        let (state, attempt, crashes): (String, i32, i32) = sqlx::query_as("SELECT state, attempt, crashes FROM jobs WHERE id = $1")
            .bind(failed).fetch_one(&pool).await.unwrap();
        assert_eq!((state.as_str(), attempt, crashes), ("available", 0, 0));
        assert!(!ops::retry(&pool, running).await.unwrap(), "running jobs are not retried");

        assert_eq!(ops::cancel(&pool, running).await.unwrap(), CancelOutcome::Requested);
        assert_eq!(ops::cancel(&pool, waiting).await.unwrap(), CancelOutcome::Cancelled);
        assert_eq!(ops::cancel(&pool, waiting).await.unwrap(), CancelOutcome::AlreadyFinished);
        assert_eq!(ops::cancel(&pool, 999_999).await.unwrap(), CancelOutcome::NotFound);
        assert!(ops::discard(&pool, failed).await.unwrap());
        assert_eq!(job_state(&pool, failed).await, "cancelled");
    }).await;
}

#[tokio::test]
async fn retry_failed_filters_by_kind_and_age_and_prune_preview_counts() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, state, max_attempts, max_crashes, deadline_s, lease_s, finished_at)
             VALUES ('a.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now() - interval '10 minutes'),
                    ('a.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now() - interval '3 days'),
                    ('b.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now()),
                    ('b.v1', 'q', '{}', 'succeeded', 3, 2, 60, 60, now() - interval '2 days')",
        ).execute(&pool).await.unwrap();
        assert_eq!(ops::retry_failed(&pool, Some("a.v1"), Some(Duration::from_secs(3600))).await.unwrap(), 1);
        assert_eq!(ops::prune_preview(&pool, Duration::from_secs(86_400), Duration::from_secs(86_400)).await.unwrap(), (1, 1));
    }).await;
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p library-jobs --test ops`
Expected: compile error.

- [ ] **Step 3: Implement `ops.rs`**

```rust
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
    let paused: Vec<String> = sqlx::query_scalar("SELECT queue FROM job_queue_pauses").fetch_all(pool).await?;
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
                failed_24h: failed_by_queue.iter().find(|(q, _)| *q == queue).map_or(0, |(_, n)| *n),
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
    Ok(Status { queues, processes: processes(pool).await?, orphan_kinds })
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
pub async fn retry(pool: &PgPool, id: i64) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = 0, crashes = 0, run_at = now(), finished_at = NULL,
                cancel_requested = false
         WHERE id = $1 AND state IN ('failed', 'cancelled')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

pub async fn retry_failed(pool: &PgPool, kind: Option<&str>, since: Option<Duration>) -> Result<u64, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = 0, crashes = 0, run_at = now(), finished_at = NULL
         WHERE state = 'failed' AND ($1::text IS NULL OR kind = $1)
           AND ($2::float8 IS NULL OR finished_at > now() - make_interval(secs => $2::float8))",
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
            let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM jobs WHERE id = $1)")
                .bind(id)
                .fetch_one(pool)
                .await?;
            if exists { CancelOutcome::AlreadyFinished } else { CancelOutcome::NotFound }
        }
    })
}

pub async fn pause(pool: &PgPool, queue: &str, by: &str, reason: Option<&str>) -> Result<(), sqlx::Error> {
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
    let r = sqlx::query("DELETE FROM job_queue_pauses WHERE queue = $1").bind(queue).execute(pool).await?;
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
pub async fn prune_preview(pool: &PgPool, keep_succeeded: Duration, keep_failed: Duration) -> Result<(i64, i64), sqlx::Error> {
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
```

Add `pub mod ops;` to `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-jobs --test ops`
Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: ops queries (status with orphan kinds, list, show, retry, discard, cancel, pause, processes, prune preview)"
```

---

### Task 14: Real `kill -9`, database outage, and throughput

**Files:**
- Create: `crates/jobs/tests/child_process.rs`, `crates/jobs/tests/outage.rs`, `crates/jobs/examples/throughput.rs`

**Interfaces:**
- Consumes: `common::*`, `Worker`, `Enqueuer`, `migrate`.

- [ ] **Step 1: Write the `kill -9` test**

Create `crates/jobs/tests/child_process.rs`:

```rust
//! A real SIGKILL: this test binary re-runs itself as a worker process.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use library_jobs::Worker;
use tokio_util::sync::CancellationToken;

/// Does nothing unless JOBS_CHILD_DB_URL is set; then it *is* the worker the
/// parent test kills.
#[tokio::test(flavor = "multi_thread")]
async fn child_worker_entry() {
    let Ok(url) = std::env::var("JOBS_CHILD_DB_URL") else { return };
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(5).connect(&url).await.unwrap();
    let mut config = fast_config("default:2");
    config.maintenance.enabled = false;
    Worker::new(pool, registry(), config).unwrap().run(CancellationToken::new()).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sigkill_mid_job_is_recovered_by_another_worker() {
    with_db_url(|pool, url| async move {
        let id = enqueue(&pool, &SleepOnce { ms: 60_000 }).await;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["child_worker_entry", "--exact", "--nocapture", "--test-threads=1"])
            .env("JOBS_CHILD_DB_URL", &url)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        eventually(Duration::from_secs(20), "child claimed the job", || async { job_state(&pool, id).await == "running" }).await;
        child.kill().unwrap(); // SIGKILL: no cleanup, no deregistration
        child.wait().unwrap();
        let w = start(&pool, fast_config("default:2"));
        eventually(Duration::from_secs(15), "recovered and succeeded", || async { job_state(&pool, id).await == "succeeded" }).await;
        let (attempt, crashes, cause): (i32, i32, String) =
            sqlx::query_as("SELECT attempt, crashes, errors->0->>'cause' FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((attempt, crashes, cause.as_str()), (2, 1, "process_lost"));
        assert_eq!(count(&pool, "SELECT count(*) FROM test_runs WHERE attempt = 2").await, 1);
        w.shutdown().await;
    }).await;
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p library-jobs --test child_process`
Expected: 2 PASS (the entry test passes instantly in the parent run). If the child cannot connect, check `TestDb.url` (the database must exist before the child starts; `with_db_url` creates it first).

- [ ] **Step 3: Write the outage test (ignored by default: it runs its own Postgres container)**

Create `crates/jobs/tests/outage.rs`:

```rust
//! Pauses a dedicated Postgres container for 20 s mid-run (spec §20 "DB outage").
//! Run with: cargo test -p library-jobs --test outage -- --ignored
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::process::Command;
use std::time::Duration;

use common::*;

const NAME: &str = "jobs-outage-test-pg";
const URL: &str = "postgres://postgres:test@127.0.0.1:55433/postgres"; // local only

fn docker(args: &[&str]) {
    let ok = Command::new("docker").args(args).status().unwrap().success();
    assert!(ok, "docker {args:?} failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker; starts, pauses and removes its own container"]
async fn workers_survive_a_database_outage() {
    let _ = Command::new("docker").args(["rm", "-f", NAME]).status();
    docker(&["run", "-d", "--name", NAME, "-e", "POSTGRES_PASSWORD=test", "-p", "127.0.0.1:55433:5432",
             "--tmpfs", "/var/lib/postgresql", "postgres:18.6"]);
    let result = tokio::spawn(async {
        let pool = loop {
            match sqlx::postgres::PgPoolOptions::new().max_connections(20).acquire_timeout(Duration::from_secs(60)).connect(URL).await {
                Ok(p) => break p,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        library_jobs::migrate(&pool).await.unwrap();
        sqlx::query("CREATE TABLE test_runs (n BIGINT NOT NULL, attempt INT NOT NULL, at TIMESTAMPTZ NOT NULL DEFAULT now())")
            .execute(&pool).await.unwrap();
        for _ in 0..200 { enqueue(&pool, &Sleepy { ms: 200, honour_cancel: true }).await; }
        let a = start(&pool, fast_config("default:4"));
        let b = start(&pool, fast_config("default:4"));
        tokio::time::sleep(Duration::from_secs(1)).await;
        docker(&["pause", NAME]);
        tokio::time::sleep(Duration::from_secs(20)).await;
        docker(&["unpause", NAME]);
        eventually(Duration::from_secs(120), "all 200 succeeded", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'succeeded'").await == 200
        }).await;
        assert!(!a.handle.is_finished() && !b.handle.is_finished(), "a worker exited during the outage");
        a.shutdown().await;
        b.shutdown().await;
    }).await;
    let _ = Command::new("docker").args(["rm", "-f", NAME]).status();
    if let Err(err) = result {
        std::panic::resume_unwind(err.into_panic());
    }
}
```

- [ ] **Step 4: Run it once**

Run: `cargo test -p library-jobs --test outage -- --ignored --nocapture`
Expected: PASS in about 30–60 s. Handlers whose leases expired during the pause are cancelled (lease lost) and later reaped and re-run; that is the intended behaviour.

- [ ] **Step 5: Write the throughput example**

Create `crates/jobs/examples/throughput.rs`:

```rust
//! No-op jobs per second with one worker process (spec §21: ≥ 1,000/s on the
//! 2-core box). Usage: cargo run --release -p library-jobs --example throughput -- <DATABASE_URL> [jobs] [slots]
//! It drops and recreates the jobs tables' rows in that database: use a scratch database.

use std::time::{Duration, Instant};

use library_jobs::{Job, JobContext, JobError, QueueConfig, Registry, Worker, WorkerConfig};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
struct Nothing;

impl Job for Nothing {
    const KIND: &'static str = "bench_nothing.v1";
    const QUEUE: &'static str = "bench";
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args.next().ok_or_else(|| anyhow::anyhow!("usage: throughput <DATABASE_URL> [jobs] [slots]"))?;
    let jobs: i64 = args.next().map_or(Ok(20_000), |s| s.parse())?;
    let slots: usize = args.next().map_or(Ok(64), |s| s.parse())?;
    let pool = sqlx::postgres::PgPoolOptions::new().max_connections(20).connect(&url).await?;
    library_jobs::migrate(&pool).await?;
    sqlx::query("DELETE FROM jobs WHERE kind = 'bench_nothing.v1'").execute(&pool).await?;
    sqlx::query(
        "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
         SELECT 'bench_nothing.v1', 'bench', 'null', 3, 2, 60, 60 FROM generate_series(1, $1)",
    )
    .bind(jobs)
    .execute(&pool)
    .await?;
    let mut config = WorkerConfig::new(vec![QueueConfig { name: "bench".into(), slots, max_running: None }]);
    config.poll = Duration::from_millis(50);
    config.maintenance.enabled = false;
    let worker = Worker::new(pool.clone(), Registry::new().register::<Nothing>(), config)?;
    let stop = CancellationToken::new();
    let handle = tokio::spawn(worker.run(stop.clone()));
    let start = Instant::now();
    loop {
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE kind = 'bench_nothing.v1' AND state <> 'succeeded'")
            .fetch_one(&pool)
            .await?;
        if left == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let secs = start.elapsed().as_secs_f64();
    stop.cancel();
    handle.await??;
    println!("{jobs} jobs in {secs:.2} s = {:.0} jobs/s ({slots} slots)", jobs as f64 / secs);
    Ok(())
}
```

`Nothing` serialises as `null`, matching the inserted `args`.

- [ ] **Step 6: Run the throughput smoke**

Run (scratch database on the test Postgres):
```bash
docker exec sqlite-production-test-pg psql -U postgres -c 'DROP DATABASE IF EXISTS jobs_bench' -c 'CREATE DATABASE jobs_bench'
taskset -c 6-9 cargo run --release -p library-jobs --example throughput -- "${TEST_POSTGRES_URL%/*}/jobs_bench"
```
Expected: a line like `20000 jobs in N s = X jobs/s`; X ≥ 1,000. Record the number for `docs/JOBS.md` (Task 19). If it is below 1,000, profile before changing anything (likely suspects: claim batch size = free slots, per-job `notify_one` wake-ups).

- [ ] **Step 7: Commit**

```bash
cargo clippy -p library-jobs --all-targets -- -D warnings && cargo fmt
git add crates/jobs
git commit -m "jobs: real SIGKILL recovery test, ignored database-outage test, throughput example"
```

---
### Task 15: Web app: jobs config, dependencies, `worker` and `jobs` subcommands, embedded mode

**Files:**
- Create: `apps/web/src/jobs_config.rs`, `apps/web/src/jobs/mod.rs`, `apps/web/src/jobs/worker.rs`, `apps/web/src/jobs/ops_cli.rs`, `apps/web/tests/jobs_worker.rs`
- Modify: `apps/web/Cargo.toml`, `apps/web/src/config.rs`, `apps/web/src/lib.rs`, `apps/web/src/cli.rs`, `apps/web/src/main.rs`, `apps/web/src/server.rs`, `apps/web/tests/common/mod.rs`

**Interfaces:**
- Consumes: `library_jobs::{Worker, WorkerConfig, QueueConfig, Registry, ExitReason, migrate, http, ops}` (Tasks 1–13).
- Produces:
  - `jobs_config::{JobsConfig, Secret, WebhookEndpoint, decode_secret}`. `JobsConfig { enabled: bool, embedded: bool, queues: String, poll_ms: u64, notify: bool, db_pool: u32, shutdown_timeout_s: u64, keep_succeeded_h: u64, keep_failed_h: u64, metrics_addr: SocketAddr, smtp_url: Option<Secret>, smtp_timeout_s: u64, email_from: String, email_domain: String, webhooks: Vec<WebhookEndpoint> }`, `JobsConfig::from_lookup(&impl Fn(&str) -> Option<String>) -> anyhow::Result<Self>`, `Default` (disabled). `Secret::new`, `Secret::expose() -> &str`, `Debug` prints `***`. `WebhookEndpoint { url: String, secret: Secret }`. `decode_secret(&str) -> anyhow::Result<Vec<u8>>`.
  - `Config.jobs: JobsConfig`; `config::parse_or` becomes `pub(crate)`.
  - `jobs::registry(&JobsConfig) -> anyhow::Result<Registry>`, `jobs::worker_config(&JobsConfig, process_kind: &str) -> anyhow::Result<WorkerConfig>`, `jobs::migrate(&PgPool)`, `jobs::connect(url, max) -> anyhow::Result<PgPool>`.
  - `jobs::worker::run(Config) -> anyhow::Result<()>`, `jobs::worker::on_serve(&Database, &Config) -> anyhow::Result<Option<Embedded>>`, `Embedded::stop(self)`.
  - `cli::JobsCommand` (below), `Command::Worker`, `Command::Jobs { command }` (both `#[cfg(feature = "postgres")]`).
  - `jobs::ops_cli::{run(Config, JobsCommand), execute(&PgPool, &JobsConfig, JobsCommand) -> anyhow::Result<String>, parse_age(&str) -> anyhow::Result<Duration>}`.
  - `server::shutdown_signal` becomes `pub(crate)`.
  - Tests: `common::on_postgres_url(|db, url| async {..})`.

- [ ] **Step 1: Dependencies**

In `apps/web/Cargo.toml` replace the `postgres` feature and add the optional dependencies (after `sha2.workspace = true`):

```toml
[features]
# Postgres backend for the load-test comparison, and background jobs (Postgres
# only). Off by default so the default web binary has no Postgres driver (spec §4).
postgres = ["sqlx/postgres", "dep:library-jobs", "dep:tokio-util", "dep:lettre", "dep:reqwest", "dep:hmac"]
```

```toml
library-jobs = { workspace = true, optional = true }
tokio-util = { version = "0.7", optional = true }
lettre = { version = "0.11", default-features = false, features = ["builder", "smtp-transport", "tokio1", "tokio1-rustls", "aws-lc-rs", "webpki-roots", "pool"], optional = true }
reqwest = { version = "0.13", default-features = false, features = ["json", "rustls"], optional = true }
hmac = { version = "0.13", optional = true }
```

Run: `cargo build -p library-web && cargo build -p library-web --features postgres && test -z "$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)"`
Expected: both build; the default build still has no `sqlx-postgres`.

- [ ] **Step 2: Write the failing config tests**

Create `apps/web/src/jobs_config.rs` with only the tests module first (the types follow in Step 4):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
    }

    // "whsec_" + base64 of 32 bytes 0..32. Test value, local only.
    const SECRET: &str = "whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

    #[test]
    fn disabled_by_default_with_documented_defaults() {
        let c = JobsConfig::from_lookup(&lookup(&[])).unwrap();
        assert_eq!(c, JobsConfig::default());
        assert!(!c.enabled && !c.embedded && c.notify);
        assert_eq!(c.queues, "email:4,webhooks:8");
        assert_eq!((c.poll_ms, c.db_pool, c.shutdown_timeout_s), (1000, 16, 25));
        assert_eq!((c.keep_succeeded_h, c.keep_failed_h, c.smtp_timeout_s), (24, 336, 30));
        assert_eq!(c.metrics_addr.to_string(), "0.0.0.0:9300");
        assert!(c.smtp_url.is_none() && c.webhooks.is_empty());
    }

    #[test]
    fn empty_values_count_as_unset() {
        let c = JobsConfig::from_lookup(&lookup(&[("JOBS_SMTP_URL", ""), ("JOBS_WEBHOOK_ENDPOINTS", "")])).unwrap();
        assert!(c.smtp_url.is_none() && c.webhooks.is_empty());
    }

    #[test]
    fn each_webhook_endpoint_needs_its_numbered_secret() {
        let ok = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_WEBHOOK_ENDPOINTS", "https://a.example/hook, http://b.example/x"),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
            ("JOBS_WEBHOOK_SECRET_2", SECRET),
        ])).unwrap();
        assert_eq!(ok.webhooks.iter().map(|w| w.url.as_str()).collect::<Vec<_>>(), ["https://a.example/hook", "http://b.example/x"]);
        let err = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_WEBHOOK_ENDPOINTS", "https://a.example/hook,https://b.example/x"),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
        ])).unwrap_err();
        assert!(err.to_string().contains("JOBS_WEBHOOK_SECRET_2"), "{err}");
    }

    #[test]
    fn bad_endpoints_and_secrets_are_refused_by_name() {
        for (url, secret, var) in [
            ("ftp://a.example/x", SECRET, "JOBS_WEBHOOK_ENDPOINTS"),
            ("https://a.example/x", "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=", "JOBS_WEBHOOK_SECRET_1"),
            ("https://a.example/x", "whsec_not base64!", "JOBS_WEBHOOK_SECRET_1"),
            ("https://a.example/x", "whsec_AAEC", "JOBS_WEBHOOK_SECRET_1"),
        ] {
            let err = JobsConfig::from_lookup(&lookup(&[("JOBS_WEBHOOK_ENDPOINTS", url), ("JOBS_WEBHOOK_SECRET_1", secret)])).unwrap_err();
            assert!(err.to_string().contains(var), "{url} {secret}: {err}");
        }
    }

    #[test]
    fn numbers_are_validated_with_the_variable_name() {
        for (var, bad) in [("JOBS_POLL_MS", "0"), ("JOBS_DB_POOL", "0"), ("JOBS_SHUTDOWN_TIMEOUT_S", "x"), ("JOBS_SMTP_TIMEOUT_S", "0")] {
            let err = JobsConfig::from_lookup(&lookup(&[(var, bad)])).unwrap_err();
            assert!(err.to_string().contains(var), "{var}: {err}");
        }
    }

    #[test]
    fn debug_output_never_shows_secrets() {
        let c = JobsConfig::from_lookup(&lookup(&[
            ("JOBS_SMTP_URL", "smtp://user:hunter2@mail.example:587"),
            ("JOBS_WEBHOOK_ENDPOINTS", "https://a.example/hook"),
            ("JOBS_WEBHOOK_SECRET_1", SECRET),
        ])).unwrap();
        let shown = format!("{c:?}");
        assert!(!shown.contains("hunter2") && !shown.contains("AAECAwQF"), "{shown}");
        assert_eq!(decode_secret(SECRET).unwrap(), (0u8..32).collect::<Vec<_>>());
    }
}
```

In `apps/web/src/config.rs` tests module, add:

```rust
    #[test]
    fn jobs_need_a_postgres_database() {
        let err = Config::from_lookup(lookup(&[("JOBS_ENABLED", "true")])).unwrap_err();
        assert!(err.to_string().contains("JOBS_ENABLED"), "{err}");
        let ok = Config::from_lookup(lookup(&[
            ("JOBS_ENABLED", "true"),
            ("DATABASE_URL", "postgres://u:p@localhost/db"), // local only
        ])).unwrap();
        assert!(ok.jobs.enabled);
    }
```

In `apps/web/src/cli.rs` tests module, add:

```rust
    #[cfg(feature = "postgres")]
    #[test]
    fn worker_and_jobs_subcommands() {
        assert_eq!(Cli::parse_from(["library-web", "worker"]).command(), Command::Worker);
        assert_eq!(
            Cli::parse_from(["library-web", "jobs", "retry", "--failed", "--kind", "webhook.v1", "--since", "2h"]).command(),
            Command::Jobs { command: JobsCommand::Retry { id: None, failed: true, kind: Some("webhook.v1".into()), since: Some("2h".into()) } }
        );
        assert!(Cli::try_parse_from(["library-web", "jobs", "retry"]).is_err(), "an id or --failed is required");
        assert!(Cli::try_parse_from(["library-web", "jobs", "list", "--state", "bogus"]).is_err());
        assert!(Cli::try_parse_from(["library-web", "jobs", "prune"]).is_err(), "only --dry-run exists");
    }
```

- [ ] **Step 3: Run to see them fail**

Run: `cargo test -p library-web --lib && cargo test -p library-web --features postgres --lib`
Expected: compile errors (`JobsConfig`, `Command::Worker` not found).

- [ ] **Step 4: Implement `jobs_config.rs`**

Above the tests module:

```rust
//! Background-job settings (docs/JOBS.md, spec §18). Plain data, compiled into
//! every build so `Config` has one shape; only the `postgres` feature acts on it.

use std::fmt;
use std::net::SocketAddr;

use anyhow::{Context, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::config::parse_or;

/// A value that must never reach a log: `Debug` prints `***`.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// An outgoing-webhook receiver. Only operators configure these, never API input
/// (spec §15.3: no SSRF surface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookEndpoint {
    pub url: String,
    /// `whsec_<base64 key>`, the Standard Webhooks format.
    pub secret: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobsConfig {
    /// `JOBS_ENABLED`: enqueue on borrow/return, migrate the jobs schema, allow `worker`.
    pub enabled: bool,
    /// `JOBS_EMBEDDED`: also run a worker inside `serve` (development, tiny deployments).
    pub embedded: bool,
    /// `JOBS_QUEUES`: `name:slots[:max_running]`, comma separated.
    pub queues: String,
    pub poll_ms: u64,
    /// `JOBS_NOTIFY`: NOTIFY on enqueue and LISTEN in workers.
    pub notify: bool,
    /// `JOBS_DB_POOL`: the worker's own pool (claims, heartbeats, handlers).
    pub db_pool: u32,
    pub shutdown_timeout_s: u64,
    pub keep_succeeded_h: u64,
    pub keep_failed_h: u64,
    /// `JOBS_METRICS_ADDR`: `/metrics`, `/healthz`, `/readyz` of `worker`.
    pub metrics_addr: SocketAddr,
    /// `JOBS_SMTP_URL`, e.g. `smtp://user:pass@host:587?tls=required`. Unset: no emails.
    pub smtp_url: Option<Secret>,
    pub smtp_timeout_s: u64,
    pub email_from: String,
    /// Right-hand side of the stable `Message-ID`.
    pub email_domain: String,
    pub webhooks: Vec<WebhookEndpoint>,
}

impl Default for JobsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            embedded: false,
            queues: "email:4,webhooks:8".to_owned(),
            poll_ms: 1000,
            notify: true,
            db_pool: 16,
            shutdown_timeout_s: 25,
            keep_succeeded_h: 24,
            keep_failed_h: 14 * 24,
            metrics_addr: SocketAddr::from(([0, 0, 0, 0], 9300)),
            smtp_url: None,
            smtp_timeout_s: 30,
            email_from: "library@localhost".to_owned(),
            email_domain: "library.local".to_owned(),
            webhooks: Vec::new(),
        }
    }
}

const MAX_WEBHOOK_ENDPOINTS: usize = 10;

impl JobsConfig {
    pub fn from_lookup(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        // An empty value is the same as an unset one (compose writes `VAR:` as "").
        let get = |key: &str| get(key).filter(|v| !v.trim().is_empty());
        let d = Self::default();
        let positive = |key: &str, default: u64| -> anyhow::Result<u64> {
            let n: u64 = parse_or(&get, key, default)?;
            if n == 0 {
                bail!("{key} must be greater than zero");
            }
            Ok(n)
        };
        let db_pool = u32::try_from(positive("JOBS_DB_POOL", u64::from(d.db_pool))?)
            .context("JOBS_DB_POOL is too large")?;
        Ok(Self {
            enabled: parse_or(&get, "JOBS_ENABLED", d.enabled)?,
            embedded: parse_or(&get, "JOBS_EMBEDDED", d.embedded)?,
            queues: get("JOBS_QUEUES").unwrap_or(d.queues),
            poll_ms: positive("JOBS_POLL_MS", d.poll_ms)?,
            notify: parse_or(&get, "JOBS_NOTIFY", d.notify)?,
            db_pool,
            shutdown_timeout_s: positive("JOBS_SHUTDOWN_TIMEOUT_S", d.shutdown_timeout_s)?,
            keep_succeeded_h: positive("JOBS_KEEP_SUCCEEDED_H", d.keep_succeeded_h)?,
            keep_failed_h: positive("JOBS_KEEP_FAILED_H", d.keep_failed_h)?,
            metrics_addr: parse_or(&get, "JOBS_METRICS_ADDR", d.metrics_addr)?,
            smtp_url: get("JOBS_SMTP_URL").map(Secret::new),
            smtp_timeout_s: positive("JOBS_SMTP_TIMEOUT_S", d.smtp_timeout_s)?,
            email_from: get("JOBS_EMAIL_FROM").unwrap_or(d.email_from),
            email_domain: get("JOBS_EMAIL_DOMAIN").unwrap_or(d.email_domain),
            webhooks: webhook_endpoints(&get)?,
        })
    }
}

fn webhook_endpoints(get: &impl Fn(&str) -> Option<String>) -> anyhow::Result<Vec<WebhookEndpoint>> {
    let Some(raw) = get("JOBS_WEBHOOK_ENDPOINTS") else {
        return Ok(Vec::new());
    };
    let urls: Vec<&str> = raw.split(',').map(str::trim).filter(|u| !u.is_empty()).collect();
    if urls.len() > MAX_WEBHOOK_ENDPOINTS {
        bail!("JOBS_WEBHOOK_ENDPOINTS lists {} endpoints; at most {MAX_WEBHOOK_ENDPOINTS}", urls.len());
    }
    urls.into_iter()
        .enumerate()
        .map(|(i, url)| {
            let ok = (url.starts_with("https://") || url.starts_with("http://"))
                && url.len() <= 2048
                && !url.chars().any(char::is_whitespace);
            if !ok {
                bail!("JOBS_WEBHOOK_ENDPOINTS: `{url}` is not an http(s) URL");
            }
            let var = format!("JOBS_WEBHOOK_SECRET_{}", i + 1);
            let secret = get(&var).with_context(|| format!("{var} is required for webhook endpoint {url}"))?;
            decode_secret(&secret).with_context(|| format!("{var} is invalid"))?;
            Ok(WebhookEndpoint { url: url.to_owned(), secret: Secret::new(secret) })
        })
        .collect()
}

/// The key bytes of a `whsec_<base64>` secret (24 to 64 bytes).
pub fn decode_secret(secret: &str) -> anyhow::Result<Vec<u8>> {
    let encoded = secret.strip_prefix("whsec_").context("the secret must start with `whsec_`")?;
    let key = STANDARD.decode(encoded).context("the part after `whsec_` must be base64")?;
    if !(24..=64).contains(&key.len()) {
        bail!("the key must be 24 to 64 bytes, got {}", key.len());
    }
    Ok(key)
}
```

In `config.rs`: make `fn parse_or` `pub(crate) fn parse_or`; add the field and its doc to `Config`:

```rust
    /// Background jobs (Postgres only; see `jobs_config`).
    pub jobs: crate::jobs_config::JobsConfig,
```

and in `from_lookup`, before `Ok(Self {`:

```rust
        let database_url = get("DATABASE_URL").unwrap_or_else(|| "sqlite://data/library.db".to_owned());
        let jobs = crate::jobs_config::JobsConfig::from_lookup(&get)?;
        if jobs.enabled && !crate::db::is_postgres_url(&database_url) {
            bail!("JOBS_ENABLED=true needs a Postgres DATABASE_URL: background jobs are Postgres only");
        }
```

then use `database_url,` and `jobs,` in the struct literal (replacing the inline `database_url: get(…)…`).

In `lib.rs` add `pub mod jobs_config;` and

```rust
#[cfg(feature = "postgres")]
pub mod jobs;
```

In `apps/web/tests/common/mod.rs`, `test_config()` gets `jobs: library_web::jobs_config::JobsConfig::default(),`.

- [ ] **Step 5: CLI**

In `cli.rs`, add to `Command` (after `CheckDb`):

```rust
    /// Run background jobs: claim loops, heartbeats, maintenance, and
    /// /metrics /healthz /readyz on JOBS_METRICS_ADDR (Postgres, JOBS_ENABLED=true).
    #[cfg(feature = "postgres")]
    Worker,
    /// Inspect and repair background jobs.
    #[cfg(feature = "postgres")]
    Jobs {
        #[command(subcommand)]
        command: JobsCommand,
    },
```

and below `Command`:

```rust
/// `library-web jobs …` (spec §17.4).
#[cfg(feature = "postgres")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum JobsCommand {
    /// Per queue: available, scheduled, running, failed in 24 h, oldest wait; processes; orphan kinds.
    Status,
    /// Jobs, newest first.
    List {
        #[arg(long, value_parser = ["available", "running", "succeeded", "failed", "cancelled"])]
        state: Option<String>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// One job with its arguments, errors and progress.
    Show { id: i64 },
    /// Failed or cancelled → available, attempts reset: one job, or `--failed` in bulk.
    #[command(group(clap::ArgGroup::new("target").required(true).args(["id", "failed"])))]
    Retry {
        id: Option<i64>,
        #[arg(long, conflicts_with = "id")]
        failed: bool,
        #[arg(long, requires = "failed")]
        kind: Option<String>,
        /// Only jobs that failed within this age: 30m, 2h, 7d.
        #[arg(long, requires = "failed")]
        since: Option<String>,
    },
    /// A waiting (or failed) job → cancelled.
    Discard { id: i64 },
    /// Cancel a waiting job now, or ask a running one to stop.
    Cancel { id: i64 },
    /// Stop claiming from a queue (running jobs finish).
    Pause {
        queue: String,
        #[arg(long)]
        reason: Option<String>,
    },
    Resume { queue: String },
    /// The process registry, with heartbeat ages.
    Processes,
    /// What retention would delete now (the leader deletes; this only counts).
    Prune {
        #[arg(long, required = true)]
        dry_run: bool,
    },
}
```

In `main.rs`, change the import to `use library_web::cli::{Cli, Command};` (unchanged) and add match arms:

```rust
        #[cfg(feature = "postgres")]
        Command::Worker => library_web::jobs::worker::run(config).await,
        #[cfg(feature = "postgres")]
        Command::Jobs { command } => library_web::jobs::ops_cli::run(config, command).await,
```

- [ ] **Step 6: `jobs/mod.rs`**

```rust
//! The library app's background jobs (docs/JOBS.md): the kinds it defines, where
//! they are enqueued, and the `worker` and `jobs` subcommands. The queue itself is
//! the generic `library-jobs` crate.

pub mod ops_cli;
pub mod worker;

use std::time::Duration;

use anyhow::Context;
use library_jobs::{QueueConfig, Registry, WorkerConfig};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::jobs_config::JobsConfig;

/// Every kind this process can run, with what the handlers need. A kind whose
/// settings are missing (no SMTP URL, no webhook endpoint) is left out, so its
/// jobs wait, and `jobs status` names them, instead of failing.
pub fn registry(jobs: &JobsConfig) -> anyhow::Result<Registry> {
    let _ = jobs; // the email and webhook kinds read it (Tasks 16-17)
    Ok(Registry::new())
}

pub fn worker_config(jobs: &JobsConfig, process_kind: &str) -> anyhow::Result<WorkerConfig> {
    let queues = QueueConfig::parse_list(&jobs.queues).context("JOBS_QUEUES")?;
    let mut config = WorkerConfig::new(queues);
    config.poll = Duration::from_millis(jobs.poll_ms);
    config.listen = jobs.notify;
    config.shutdown_timeout = Duration::from_secs(jobs.shutdown_timeout_s);
    config.process_kind = process_kind.to_owned();
    config.version = env!("CARGO_PKG_VERSION").to_owned();
    config.maintenance.keep_succeeded = Duration::from_secs(jobs.keep_succeeded_h * 3600);
    config.maintenance.keep_failed = Duration::from_secs(jobs.keep_failed_h * 3600);
    config.validate()?;
    Ok(config)
}

/// The jobs schema, after the app's (spec §3.3).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    library_jobs::migrate(pool).await.context("jobs migrations")
}

/// A pool of its own, so job work never takes the API's connections.
pub async fn connect(database_url: &str, max_connections: u32) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
        .context("connecting to DATABASE_URL")
}
```

- [ ] **Step 7: `jobs/worker.rs`**

```rust
//! `library-web worker`, and the worker embedded in `serve` (spec §3.2).

use anyhow::{Context, bail};
use library_jobs::{ExitReason, Worker};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::{self, Database};

pub async fn run(config: Config) -> anyhow::Result<()> {
    if !config.jobs.enabled {
        bail!("JOBS_ENABLED is not true: the worker has nothing to do");
    }
    let pool = super::connect(&config.database_url, config.jobs.db_pool).await?;
    db::migrate(&Database::Postgres(pool.clone())).await?;
    super::migrate(&pool).await?;
    let worker_config = super::worker_config(&config.jobs, "worker")?;
    let heartbeat = worker_config.heartbeat;
    let worker = Worker::new(pool.clone(), super::registry(&config.jobs)?, worker_config)?;

    let stop = CancellationToken::new();
    let router = library_jobs::http::router(worker.metrics(), worker.health(), pool.clone(), heartbeat);
    let addr = config.jobs.metrics_addr;
    let listener = TcpListener::bind(addr).await.with_context(|| format!("binding JOBS_METRICS_ADDR {addr}"))?;
    tracing::info!(%addr, "worker metrics and health listening");
    let http = tokio::spawn(library_jobs::http::serve(listener, router, stop.clone()));
    let signal = {
        let stop = stop.clone();
        tokio::spawn(async move {
            crate::server::shutdown_signal().await;
            stop.cancel();
        })
    };

    let reason = worker.run(stop.clone()).await;
    stop.cancel();
    signal.abort();
    let _ = http.await;
    pool.close().await;
    match reason? {
        ExitReason::Shutdown => Ok(()),
        ExitReason::SelfFenced => {
            bail!("the worker fenced itself off (its registry row was deleted); exiting so the supervisor restarts it")
        }
    }
}

/// A worker running inside `serve` (`JOBS_EMBEDDED=true`).
pub struct Embedded {
    stop: CancellationToken,
    handle: JoinHandle<()>,
}

impl Embedded {
    /// Graceful stop: finish or release running jobs (spec §13).
    pub async fn stop(self) {
        self.stop.cancel();
        let _ = self.handle.await;
    }
}

/// What `serve` does about jobs: nothing when disabled; otherwise migrate the jobs
/// schema and, if embedded, start a worker on a pool of its own.
pub async fn on_serve(database: &Database, config: &Config) -> anyhow::Result<Option<Embedded>> {
    if !config.jobs.enabled {
        return Ok(None);
    }
    let Database::Postgres(pool) = database else {
        bail!("JOBS_ENABLED=true needs a Postgres DATABASE_URL");
    };
    super::migrate(pool).await?;
    if !config.jobs.embedded {
        return Ok(None);
    }
    let pool = super::connect(&config.database_url, config.jobs.db_pool).await?;
    let worker = Worker::new(pool.clone(), super::registry(&config.jobs)?, super::worker_config(&config.jobs, "embedded")?)?;
    let stop = CancellationToken::new();
    let handle = {
        let stop = stop.clone();
        tokio::spawn(async move {
            match worker.run(stop).await {
                Ok(ExitReason::Shutdown) => {}
                Ok(ExitReason::SelfFenced) => tracing::error!("embedded worker fenced itself off; jobs stop until restart"),
                Err(err) => tracing::error!(error = %err, "embedded worker failed"),
            }
            pool.close().await;
        })
    };
    tracing::info!("embedded jobs worker started");
    Ok(Some(Embedded { stop, handle }))
}
```

In `server.rs`, make `async fn shutdown_signal()` `pub(crate) async fn shutdown_signal()`, and in `serve` add after `db::migrate(&database).await?;`:

```rust
    #[cfg(feature = "postgres")]
    let embedded_jobs = crate::jobs::worker::on_serve(&database, &config).await?;
```

and before `database.close().await;`:

```rust
    #[cfg(feature = "postgres")]
    if let Some(jobs) = embedded_jobs {
        jobs.stop().await;
    }
```

- [ ] **Step 8: `jobs/ops_cli.rs`**

```rust
//! `library-web jobs …`: read and repair the job tables (spec §17.4).

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::{Context, bail};
use library_jobs::ops::{self, CancelOutcome, ListFilter, ProcessRow};
use sqlx::PgPool;

use crate::cli::JobsCommand;
use crate::config::Config;
use crate::jobs_config::JobsConfig;

/// A process this long without a heartbeat is shown as STALE (3 × the 20 s period).
const STALE_AFTER_S: f64 = 60.0;

pub async fn run(config: Config, command: JobsCommand) -> anyhow::Result<()> {
    let pool = super::connect(&config.database_url, 2).await?;
    let out = execute(&pool, &config.jobs, command).await;
    pool.close().await;
    print!("{}", out?);
    Ok(())
}

/// Run one command and return what to print.
pub async fn execute(pool: &PgPool, jobs: &JobsConfig, command: JobsCommand) -> anyhow::Result<String> {
    let mut out = String::new();
    match command {
        JobsCommand::Status => {
            let s = ops::status(pool).await?;
            writeln!(out, "{:<16} {:>9} {:>9} {:>7} {:>10} {:>9}  paused", "queue", "available", "scheduled", "running", "failed 24h", "oldest s")?;
            for q in &s.queues {
                writeln!(
                    out,
                    "{:<16} {:>9} {:>9} {:>7} {:>10} {:>9.1}  {}",
                    q.queue, q.available, q.scheduled, q.running, q.failed_24h, q.oldest_available_s,
                    if q.paused { "yes" } else { "" }
                )?;
            }
            if s.queues.is_empty() {
                writeln!(out, "(no waiting, running or recently failed jobs)")?;
            }
            writeln!(out)?;
            write_processes(&mut out, &s.processes)?;
            if !s.orphan_kinds.is_empty() {
                writeln!(out, "\nWARNING: no live process can run: {}", s.orphan_kinds.join(", "))?;
            }
        }
        JobsCommand::List { state, queue, kind, limit } => {
            let rows = ops::list(pool, &ListFilter { state, queue, kind, limit }).await?;
            writeln!(out, "{:>10}  {:<16} {:<10} {:<9} {:>7} {:>7}  {:<20}  last error", "id", "kind", "queue", "state", "attempt", "crashes", "run at")?;
            for j in rows {
                writeln!(
                    out,
                    "{:>10}  {:<16} {:<10} {:<9} {:>3}/{:<3} {:>7}  {:<20}  {}",
                    j.id, j.kind, j.queue, j.state, j.attempt, j.max_attempts, j.crashes,
                    j.run_at.format("%Y-%m-%d %H:%M:%S"),
                    j.last_error.as_deref().unwrap_or("").chars().take(80).collect::<String>()
                )?;
            }
        }
        JobsCommand::Show { id } => {
            let Some(j) = ops::show(pool, id).await? else { bail!("job {id} not found") };
            let s = &j.summary;
            writeln!(out, "id {}  kind {}  queue {}  state {}", s.id, s.kind, s.queue, s.state)?;
            writeln!(out, "attempt {}/{}  crashes {}  cancel requested {}", s.attempt, s.max_attempts, s.crashes, j.cancel_requested)?;
            writeln!(out, "created {}  run at {}  finished {:?}", s.created_at, s.run_at, s.finished_at)?;
            writeln!(out, "locked by {:?} until {:?}", j.locked_by, j.locked_until)?;
            writeln!(out, "unique key {:?}  concurrency key {:?}", j.unique_key, j.concurrency_key)?;
            writeln!(out, "args {}", serde_json::to_string_pretty(&j.args)?)?;
            writeln!(out, "progress {}", j.progress.map_or_else(|| "-".to_owned(), |p| p.to_string()))?;
            writeln!(out, "errors {}", serde_json::to_string_pretty(&j.errors)?)?;
        }
        JobsCommand::Retry { id: Some(id), .. } => {
            let done = ops::retry(pool, id).await?;
            writeln!(out, "{}", if done { format!("job {id} is available again") } else { format!("job {id} is not failed or cancelled") })?;
        }
        JobsCommand::Retry { id: None, kind, since, .. } => {
            let since = since.as_deref().map(parse_age).transpose()?;
            let n = ops::retry_failed(pool, kind.as_deref(), since).await?;
            writeln!(out, "{n} failed jobs are available again")?;
        }
        JobsCommand::Discard { id } => {
            let done = ops::discard(pool, id).await?;
            writeln!(out, "{}", if done { format!("job {id} cancelled") } else { format!("job {id} is not waiting or failed") })?;
        }
        JobsCommand::Cancel { id } => {
            let msg = match ops::cancel(pool, id).await? {
                CancelOutcome::Cancelled => "cancelled",
                CancelOutcome::Requested => "running: cancellation requested, its handler stops within one heartbeat",
                CancelOutcome::AlreadyFinished => "already finished",
                CancelOutcome::NotFound => "not found",
            };
            writeln!(out, "job {id}: {msg}")?;
        }
        JobsCommand::Pause { queue, reason } => {
            let by = std::env::var("USER").unwrap_or_else(|_| "cli".to_owned());
            ops::pause(pool, &queue, &by, reason.as_deref()).await?;
            writeln!(out, "queue {queue} paused; running jobs finish, nothing new is claimed")?;
        }
        JobsCommand::Resume { queue } => {
            let done = ops::resume(pool, &queue).await?;
            writeln!(out, "{}", if done { format!("queue {queue} resumed") } else { format!("queue {queue} was not paused") })?;
        }
        JobsCommand::Processes => write_processes(&mut out, &ops::processes(pool).await?)?,
        JobsCommand::Prune { .. } => {
            let keep_s = Duration::from_secs(jobs.keep_succeeded_h * 3600);
            let keep_f = Duration::from_secs(jobs.keep_failed_h * 3600);
            let (succeeded, failed) = ops::prune_preview(pool, keep_s, keep_f).await?;
            writeln!(out, "retention would delete {succeeded} succeeded and {failed} failed/cancelled jobs (the leader does it every minute)")?;
        }
    }
    Ok(out)
}

fn write_processes(out: &mut String, processes: &[ProcessRow]) -> anyhow::Result<()> {
    writeln!(out, "{:>6}  {:<40} {:<9} {:>8}  {:>9}  state", "id", "name", "kind", "pid", "beat ago")?;
    for p in processes {
        let state = if p.heartbeat_age_s > STALE_AFTER_S { "STALE" } else if p.stopping { "stopping" } else { "live" };
        writeln!(out, "{:>6}  {:<40} {:<9} {:>8}  {:>8.1}s  {}", p.id, p.name, p.kind, p.pid, p.heartbeat_age_s, state)?;
    }
    if processes.is_empty() {
        writeln!(out, "(no worker processes)")?;
    }
    Ok(())
}

/// `45s`, `30m`, `2h`, `7d`.
pub fn parse_age(raw: &str) -> anyhow::Result<Duration> {
    let raw = raw.trim();
    let split = raw.len().saturating_sub(1);
    let (n, unit) = raw.split_at(split);
    let n: u64 = n.parse().with_context(|| format!("`{raw}` is not an age like 30m, 2h or 7d"))?;
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        _ => bail!("`{raw}` is not an age like 30m, 2h or 7d"),
    };
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(parse_age("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_age("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_age("7d").unwrap(), Duration::from_secs(604_800));
        for bad in ["", "h", "2", "2w", "-1h", "1.5h"] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
    }
}
```

- [ ] **Step 9: Write the end-to-end worker test**

In `apps/web/tests/common/mod.rs`, turn `on_postgres` into a wrapper around a new `on_postgres_url` that also hands over the database URL:

```rust
/// Like [`on_postgres`], also passing the database's URL (for tests that start
/// the `library-web` binary against it).
#[cfg(feature = "postgres")]
pub async fn on_postgres_url<F, Fut>(body: F)
where
    F: FnOnce(Database, String) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let pg = TestPg::new().await;
    let url = {
        let base = std::env::var("TEST_POSTGRES_URL").expect("TEST_POSTGRES_URL");
        let (server, _) = base.rsplit_once('/').expect("TEST_POSTGRES_URL ends in /<database>");
        format!("{server}/{}", pg.name)
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(16)
        .connect_with(pg.options.clone())
        .await
        .expect("connect to test database");
    let db = Database::Postgres(pool.clone());
    library_web::db::migrate(&db).await.expect("migrate");
    let outcome = tokio::spawn(body(db, url)).await;
    pool.close().await;
    pg.cleanup().await;
    if let Err(err) = outcome {
        match err.try_into_panic() {
            Ok(panic) => std::panic::resume_unwind(panic),
            Err(err) => panic!("test task failed: {err}"),
        }
    }
}

#[cfg(feature = "postgres")]
pub async fn on_postgres<F, Fut>(body: F)
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    on_postgres_url(|db, _url| body(db)).await;
}
```

(Replace the old body of `on_postgres` and keep its doc comment.)

Create `apps/web/tests/jobs_worker.rs`:

```rust
//! The `worker` and `jobs` subcommands of the real binary, against a fresh database.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

fn bin(url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_library-web"));
    // Outside the repo, so no .env file is picked up.
    cmd.current_dir(std::env::temp_dir())
        .env("DATABASE_URL", url)
        .env("JOBS_ENABLED", "true")
        .env("RUST_LOG", "warn")
        .env_remove("JOBS_SMTP_URL")
        .env_remove("JOBS_WEBHOOK_ENDPOINTS");
    cmd
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    out
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

async fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(start.elapsed() < within, "the worker did not exit within {within:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_becomes_ready_and_stops_cleanly_on_sigterm() {
    common::on_postgres_url(|db, url| async move {
        let library_web::db::Database::Postgres(pool) = db else { panic!("postgres only") };
        let port = free_port();
        let mut child = bin(&url)
            .arg("worker")
            .env("JOBS_METRICS_ADDR", format!("127.0.0.1:{port}"))
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let ready = format!("http://127.0.0.1:{port}/readyz");
        let start = Instant::now();
        while !reqwest::get(&ready).await.map(|r| r.status().is_success()).unwrap_or(false) {
            assert!(start.elapsed() < Duration::from_secs(30), "not ready in 30 s");
            assert!(child.try_wait().unwrap().is_none(), "the worker exited early");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics")).await.unwrap().text().await.unwrap();
        assert!(metrics.contains("jobs_leader"), "{metrics}");

        let processes = run(bin(&url).args(["jobs", "processes"]));
        assert!(String::from_utf8_lossy(&processes.stdout).contains("live"));
        let status = run(bin(&url).args(["jobs", "status"]));
        assert!(String::from_utf8_lossy(&status.stdout).contains("no waiting"));

        run(Command::new("kill").args(["-TERM", &child.id().to_string()]));
        assert!(wait_exit(&mut child, Duration::from_secs(30)).await.success());
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM job_processes").fetch_one(&pool).await.unwrap();
        assert_eq!(left, 0, "deregistered on shutdown");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_refuses_to_start_without_jobs_enabled() {
    common::on_postgres_url(|_db, url| async move {
        let out = bin(&url).arg("worker").env("JOBS_ENABLED", "false").output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("JOBS_ENABLED"));
    })
    .await;
}
```

- [ ] **Step 10: Run everything**

```bash
export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh)
cargo test -p library-web
cargo test -p library-web --features postgres
cargo clippy -p library-web --all-targets -- -D warnings
cargo clippy -p library-web --all-targets --features postgres -- -D warnings
test -z "$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)"
```
Expected: all PASS (the new unit tests, `jobs_worker` 2 tests, every existing test), both clippy runs clean, no Postgres driver in the default build.

- [ ] **Step 11: Commit**

```bash
cargo fmt
git add apps/web Cargo.lock
git commit -m "web: JOBS_* config, worker and jobs subcommands, embedded mode, jobs migrations on serve"
```

---

### Task 16: `loan_email.v1` and the Mailer

**Files:**
- Create: `apps/web/src/jobs/events.rs`, `apps/web/src/jobs/loan_email.rs`, `scripts/test-mailpit.sh`, `apps/web/tests/jobs_email.rs`, `apps/web/tests/jobs_email_chaos.rs`
- Modify: `apps/web/src/jobs/mod.rs`

**Interfaces:**
- Consumes: `Job`, `JobContext::detached`, `JobError`, `Deps` (Task 3); `JobsConfig`, `Secret` (Task 15).
- Produces:
  - `jobs::events::LoanEventKind { Borrowed, Returned }` (serde `snake_case`) with `as_str()`; `jobs::events::event_id(loan_id: i64, LoanEventKind) -> String` = `loan.<id>.<borrowed|returned>`.
  - `jobs::loan_email::LoanEmail { loan_id: i64, event: LoanEventKind }` (kind `loan_email.v1`, queue `email`, unique key = the event id).
  - `jobs::loan_email::Mailer::new(smtp_url: &str, from: &str, domain: &str, timeout: Duration) -> anyhow::Result<Mailer>`, `Mailer::from_config(&JobsConfig) -> anyhow::Result<Option<Mailer>>`, `message(job_id, to_name, to_email, subject, body) -> Result<Message, JobError>`, `send(Message) -> Result<(), JobError>`.
  - `scripts/test-mailpit.sh` prints `export TEST_SMTP_URL=…` and `export TEST_MAILPIT_URL=…`.

- [ ] **Step 1: The Mailpit script**

Create `scripts/test-mailpit.sh` and `chmod +x` it:

```bash
#!/usr/bin/env bash
# Start (or reuse) a throwaway Mailpit (an SMTP sink with an HTTP API) for the
# email job tests and print the variables they read:
#   eval "$(./scripts/test-mailpit.sh)"
# Chaos is on so a test can make it answer 4xx or 5xx (tests/jobs_email_chaos.rs).
set -euo pipefail
NAME=sqlite-production-test-mailpit
SMTP_PORT=55025
HTTP_PORT=58025
IMAGE=axllent/mailpit:v1.31.2

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" -e MP_ENABLE_CHAOS=true -e MP_MAX_MESSAGES=5000 \
    -p "127.0.0.1:$SMTP_PORT:1025" -p "127.0.0.1:$HTTP_PORT:8025" "$IMAGE" >/dev/null
fi

for _ in $(seq 1 60); do
  if curl -fsS "http://127.0.0.1:$HTTP_PORT/api/v1/info" >/dev/null 2>&1; then
    echo "export TEST_SMTP_URL=smtp://127.0.0.1:$SMTP_PORT"
    echo "export TEST_MAILPIT_URL=http://127.0.0.1:$HTTP_PORT"
    exit 0
  fi
  sleep 0.5
done
echo "test Mailpit did not become ready" >&2
exit 1
```

Run: `eval "$(./scripts/test-mailpit.sh)" && curl -fsS "$TEST_MAILPIT_URL/api/v1/chaos"`
Expected: a JSON object of chaos triggers (Sender, Recipient, Authentication). If the field names differ from `ErrorCode` / `Probability`, use the names it shows in Step 2's `chaos()` helper.

- [ ] **Step 2: Write the failing tests**

Create `apps/web/tests/jobs_email.rs`:

```rust
//! `loan_email.v1` against Mailpit (`eval "$(./scripts/test-mailpit.sh)"`).
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::db::Database;
use library_web::jobs::events::LoanEventKind;
use library_web::jobs::loan_email::{LoanEmail, Mailer};
use serde_json::Value;
use sqlx::PgPool;

pub fn smtp_url() -> String {
    std::env::var("TEST_SMTP_URL").expect("TEST_SMTP_URL is not set: eval \"$(./scripts/test-mailpit.sh)\"")
}

pub fn mailpit_url() -> String {
    std::env::var("TEST_MAILPIT_URL").expect("TEST_MAILPIT_URL is not set: eval \"$(./scripts/test-mailpit.sh)\"")
}

fn pg(db: Database) -> PgPool {
    match db {
        Database::Postgres(pool) => pool,
        _ => panic!("postgres only"),
    }
}

/// A member with an address no other test uses (Mailpit is shared), a book and a loan.
async fn loan(pool: &PgPool, email: Option<&str>, returned: bool) -> (i64, String) {
    let email = email.map_or_else(|| format!("m{}@example.test", rand::random::<u64>()), str::to_owned);
    let member: i64 = sqlx::query_scalar("INSERT INTO members (name, email, created_at, updated_at) VALUES ('Ann Reader', $1, 0, 0) RETURNING id")
        .bind(&email).fetch_one(pool).await.unwrap();
    let book: i64 = sqlx::query_scalar("INSERT INTO books (title, author, total_copies, created_at, updated_at) VALUES ('Dune', 'Frank Herbert', 1, 0, 0) RETURNING id")
        .fetch_one(pool).await.unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES ($1, $2, 1800000000, 1801209600, $3) RETURNING id",
    ).bind(book).bind(member).bind(returned.then_some(1_800_100_000_i64)).fetch_one(pool).await.unwrap();
    (id, email)
}

fn ctx(pool: &PgPool, job_id: i64, mailer: Mailer) -> JobContext {
    let mut deps = Deps::new();
    deps.insert(Arc::new(mailer));
    JobContext::detached(pool.clone(), job_id, 1, LoanEmail::KIND, deps)
}

fn mailer(url: &str, timeout: Duration) -> Mailer {
    Mailer::new(url, "Library <library@library.test>", "library.test", timeout).unwrap()
}

/// Messages Mailpit holds for `to`, waiting up to 5 s for `want` of them.
async fn inbox(to: &str, want: usize) -> Vec<Value> {
    let url = reqwest::Url::parse_with_params(&format!("{}/api/v1/search", mailpit_url()), &[("query", format!("to:{to}"))]).unwrap();
    let start = Instant::now();
    loop {
        let body: Value = reqwest::get(url.clone()).await.unwrap().json().await.unwrap();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        if messages.len() >= want || start.elapsed() > Duration::from_secs(5) {
            return messages;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resend_carries_the_same_message_id() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, to) = loan(&pool, None, false).await;
        let job = LoanEmail { loan_id, event: LoanEventKind::Borrowed };
        // Twice, as after a crash between the server's 250 and our completion write.
        job.clone().perform(ctx(&pool, 4242, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap();
        job.perform(ctx(&pool, 4242, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap();
        let messages = inbox(&to, 2).await;
        assert_eq!(messages.len(), 2);
        for m in &messages {
            assert_eq!(m["MessageID"].as_str().unwrap().trim_matches(['<', '>']), "loan_email.v1.4242@library.test");
            assert_eq!(m["Subject"], "You borrowed \"Dune\"");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_for_a_deleted_or_already_returned_loan() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let gone = LoanEmail { loan_id: 999_999, event: LoanEventKind::Borrowed };
        gone.perform(ctx(&pool, 1, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap();
        let (loan_id, to) = loan(&pool, None, true).await;
        let stale = LoanEmail { loan_id, event: LoanEventKind::Borrowed };
        stale.perform(ctx(&pool, 2, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(inbox(&to, 0).await.is_empty());
        let returned = LoanEmail { loan_id, event: LoanEventKind::Returned };
        returned.perform(ctx(&pool, 3, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap();
        assert_eq!(inbox(&to, 1).await[0]["Subject"], "You returned \"Dune\"");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_member_address_is_permanent() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, Some("not-an-address"), false).await;
        let err = LoanEmail { loan_id, event: LoanEventKind::Borrowed }
            .perform(ctx(&pool, 5, mailer(&smtp_url(), Duration::from_secs(10)))).await.unwrap_err();
        assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hanging_smtp_server_times_out_as_retry() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, None, false).await;
        // Accepts connections and never says a word.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await { held.push(socket); }
        });
        let start = Instant::now();
        let err = LoanEmail { loan_id, event: LoanEventKind::Borrowed }
            .perform(ctx(&pool, 6, mailer(&format!("smtp://127.0.0.1:{port}"), Duration::from_secs(1)))).await.unwrap_err();
        assert!(matches!(err, JobError::Retry(_)), "{err:?}");
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_is_retry() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, None, false).await;
        let err = LoanEmail { loan_id, event: LoanEventKind::Borrowed }
            .perform(ctx(&pool, 7, mailer("smtp://127.0.0.1:1", Duration::from_secs(2)))).await.unwrap_err();
        assert!(matches!(err, JobError::Retry(_)), "{err:?}");
    })
    .await;
}
```

Create `apps/web/tests/jobs_email_chaos.rs` (its own binary: chaos is global to the Mailpit container, and cargo runs test binaries one at a time):

```rust
//! SMTP replies decide retry vs permanent. One test only: it switches Mailpit's
//! global chaos triggers, which would disturb any test running beside it.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::db::Database;
use library_web::jobs::events::LoanEventKind;
use library_web::jobs::loan_email::{LoanEmail, Mailer};
use serde_json::json;

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set: eval \"$(./scripts/test-mailpit.sh)\""))
}

async fn chaos(recipient_code: u16, probability: u8) {
    let body = json!({ "Recipient": { "ErrorCode": recipient_code, "Probability": probability } });
    reqwest::Client::new()
        .put(format!("{}/api/v1/chaos", var("TEST_MAILPIT_URL")))
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_5xx_is_permanent_and_4xx_is_retry() {
    common::on_postgres(|db| async move {
        let Database::Postgres(pool) = db else { panic!("postgres only") };
        let member: i64 = sqlx::query_scalar("INSERT INTO members (name, email, created_at, updated_at) VALUES ('C', 'chaos@example.test', 0, 0) RETURNING id")
            .fetch_one(&pool).await.unwrap();
        let book: i64 = sqlx::query_scalar("INSERT INTO books (title, author, total_copies, created_at, updated_at) VALUES ('T', 'A', 1, 0, 0) RETURNING id")
            .fetch_one(&pool).await.unwrap();
        let loan_id: i64 = sqlx::query_scalar("INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1, $2, 0, 100) RETURNING id")
            .bind(book).bind(member).fetch_one(&pool).await.unwrap();
        let send = || {
            let mut deps = Deps::new();
            deps.insert(Arc::new(Mailer::new(&var("TEST_SMTP_URL"), "l@library.test", "library.test", Duration::from_secs(10)).unwrap()));
            LoanEmail { loan_id, event: LoanEventKind::Borrowed }.perform(JobContext::detached(pool.clone(), 9, 1, LoanEmail::KIND, deps))
        };
        let outcome = tokio::spawn(async move {
            chaos(550, 100).await;
            let permanent = send().await;
            chaos(451, 100).await;
            let retry = send().await;
            (permanent, retry)
        })
        .await;
        chaos(0, 0).await; // always switch chaos off again
        let (permanent, retry) = outcome.unwrap();
        assert!(matches!(permanent, Err(JobError::Permanent(_))), "{permanent:?}");
        assert!(matches!(retry, Err(JobError::Retry(_))), "{retry:?}");
    })
    .await;
}
```

If `chaos(0, 0)` is refused (Mailpit may reject `ErrorCode` 0), reset with `ErrorCode: 451, Probability: 0` instead.

- [ ] **Step 3: Run to see them fail**

Run: `eval "$(./scripts/test-mailpit.sh)" && cargo test -p library-web --features postgres --test jobs_email`
Expected: compile errors (`jobs::events`, `jobs::loan_email` not found).

- [ ] **Step 4: Implement `events.rs` (event kinds only; Task 18 adds the enqueueing)**

```rust
//! Loan events: what borrowing or returning a book tells the outside world
//! (spec §15.1).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoanEventKind {
    Borrowed,
    Returned,
}

impl LoanEventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Borrowed => "borrowed",
            Self::Returned => "returned",
        }
    }
}

/// `loan.<id>.<borrowed|returned>`: the same for every job and attempt of one
/// event, so receivers can de-duplicate on it.
pub fn event_id(loan_id: i64, kind: LoanEventKind) -> String {
    format!("loan.{loan_id}.{}", kind.as_str())
}
```

- [ ] **Step 5: Implement `loan_email.rs`**

```rust
//! `loan_email.v1`: tell the member about a borrow or a return (spec §15.2).

use std::time::Duration;

use anyhow::Context;
use chrono::DateTime;
use lettre::message::header::ContentType;
use lettre::message::{Mailbox, Message};
use lettre::{Address, AsyncSmtpTransport, AsyncTransport, Tokio1Executor};
use library_jobs::{Job, JobContext, JobError, JobOptions};
use serde::{Deserialize, Serialize};

use super::events::{LoanEventKind, event_id};
use crate::jobs_config::JobsConfig;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoanEmail {
    pub loan_id: i64,
    pub event: LoanEventKind,
}

/// What the email says, read when it is sent: it reflects the loan as it is now.
#[derive(sqlx::FromRow)]
struct LoanRow {
    member_name: String,
    member_email: String,
    book_title: String,
    due_at: i64,
    returned_at: Option<i64>,
}

impl Job for LoanEmail {
    const KIND: &'static str = "loan_email.v1";
    const QUEUE: &'static str = "email";

    fn options() -> JobOptions {
        // The SMTP timeout is 30 s by default; two minutes leaves room for a slow server.
        JobOptions { deadline: Duration::from_secs(120), ..JobOptions::default() }
    }

    fn unique_key(&self) -> Option<String> {
        Some(event_id(self.loan_id, self.event))
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        let mailer = ctx.require::<Mailer>()?;
        let row: Option<LoanRow> = sqlx::query_as(
            "SELECT m.name AS member_name, m.email AS member_email, b.title AS book_title, l.due_at, l.returned_at
             FROM loans l JOIN members m ON m.id = l.member_id JOIN books b ON b.id = l.book_id
             WHERE l.id = $1",
        )
        .bind(self.loan_id)
        .fetch_optional(ctx.pool())
        .await?;
        let Some(row) = row else {
            tracing::info!(loan_id = self.loan_id, "loan_email: the loan is gone; nothing to send");
            return Ok(());
        };
        if self.event == LoanEventKind::Borrowed && row.returned_at.is_some() {
            tracing::info!(loan_id = self.loan_id, "loan_email: already returned; the borrow email is stale");
            return Ok(());
        }
        let (subject, body) = compose(self.event, &row);
        let message = mailer.message(ctx.id(), &row.member_name, &row.member_email, &subject, body)?;
        mailer.send(message).await
    }
}

fn date(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0).map_or_else(|| "unknown".to_owned(), |d| d.format("%Y-%m-%d").to_string())
}

fn compose(event: LoanEventKind, row: &LoanRow) -> (String, String) {
    let (name, title) = (&row.member_name, &row.book_title);
    match event {
        LoanEventKind::Borrowed => (
            format!("You borrowed \"{title}\""),
            format!("Hello {name},\n\nyou borrowed \"{title}\". Please return it by {}.\n\nThe library\n", date(row.due_at)),
        ),
        LoanEventKind::Returned => (
            format!("You returned \"{title}\""),
            format!("Hello {name},\n\nthank you for returning \"{title}\".\n\nThe library\n"),
        ),
    }
}

/// SMTP sender shared by every email job of a process (pooled connections).
pub struct Mailer {
    transport: AsyncSmtpTransport<Tokio1Executor>,
    from: Mailbox,
    domain: String,
}

impl Mailer {
    /// `smtp_url`: `smtp://host:port` (plain), `smtp://user:pass@host:587?tls=required`
    /// (STARTTLS) or `smtps://…` (TLS). `timeout` bounds connecting and every reply.
    pub fn new(smtp_url: &str, from: &str, domain: &str, timeout: Duration) -> anyhow::Result<Self> {
        let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(smtp_url)
            .context("JOBS_SMTP_URL is not a valid SMTP URL")?
            .timeout(Some(timeout))
            .build();
        let from = from.parse().with_context(|| format!("JOBS_EMAIL_FROM `{from}` is not an email address"))?;
        Ok(Self { transport, from, domain: domain.to_owned() })
    }

    /// `None` when no SMTP URL is configured: the email kind is then not registered.
    pub fn from_config(jobs: &JobsConfig) -> anyhow::Result<Option<Self>> {
        jobs.smtp_url
            .as_ref()
            .map(|url| Self::new(url.expose(), &jobs.email_from, &jobs.email_domain, Duration::from_secs(jobs.smtp_timeout_s)))
            .transpose()
    }

    /// The `Message-ID` comes from the job id, so every attempt sends the same one
    /// and a receiver can drop a resend after a crash (spec §15.2).
    pub fn message(&self, job_id: i64, to_name: &str, to_email: &str, subject: &str, body: String) -> Result<Message, JobError> {
        let address: Address = to_email
            .parse()
            .map_err(|err| JobError::permanent(format!("member address `{to_email}`: {err}")))?;
        Message::builder()
            .message_id(Some(format!("<loan_email.v1.{job_id}@{}>", self.domain)))
            .from(self.from.clone())
            .to(Mailbox::new(Some(to_name.to_owned()), address))
            .subject(subject)
            .header(ContentType::TEXT_PLAIN)
            .body(body)
            .map_err(JobError::permanent)
    }

    pub async fn send(&self, message: Message) -> Result<(), JobError> {
        self.transport.send(message).await.map(drop).map_err(classify)
    }
}

/// 5xx replies are permanent (bad mailbox, rejected sender). Everything else
/// (4xx, timeouts, refused connections, TLS errors) may pass on a later attempt.
fn classify(err: lettre::transport::smtp::Error) -> JobError {
    if err.is_permanent() { JobError::permanent(err) } else { JobError::retry(err) }
}
```

In `jobs/mod.rs`: add `pub mod events;` and `pub mod loan_email;` to the module list, `use std::sync::Arc;`, and replace `registry`:

```rust
pub fn registry(jobs: &JobsConfig) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();
    if let Some(mailer) = loan_email::Mailer::from_config(jobs)? {
        registry = registry.register::<loan_email::LoanEmail>().provide(Arc::new(mailer));
    }
    Ok(registry)
}
```

If the first TLS connection panics with "no process-level CryptoProvider", two rustls providers are compiled in (lettre and reqwest); then call `let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();` at the top of `jobs::worker::run` and `on_serve` (add `rustls = { version = "0.23", default-features = false, features = ["aws-lc-rs"], optional = true }` under the `postgres` feature). Do this only if the panic appears.

- [ ] **Step 6: Run the tests**

Run: `eval "$(./scripts/test-mailpit.sh)" && cargo test -p library-web --features postgres --test jobs_email --test jobs_email_chaos`
Expected: 6 PASS.

- [ ] **Step 7: Commit**

```bash
cargo clippy -p library-web --all-targets --features postgres -- -D warnings && cargo fmt
git add apps/web scripts/test-mailpit.sh Cargo.lock
git commit -m "web: loan_email.v1 over SMTP with a stable Message-ID; 5xx permanent, the rest retried"
```

---

### Task 17: `webhook.v1`, `WebhookSender` and Standard Webhooks signatures

**Files:**
- Create: `apps/web/src/jobs/webhook.rs`, `apps/web/tests/jobs_webhook.rs`
- Modify: `apps/web/src/jobs/mod.rs`

**Interfaces:**
- Consumes: `LoanEventKind`, `event_id` (Task 16); `WebhookEndpoint`, `Secret`, `decode_secret` (Task 15); `LoanDetails` (`crate::loans`).
- Produces: `jobs::webhook::{WebhookEvent { id, kind (serde "type"), occurred_at: DateTime<Utc>, data: Value }, WebhookEvent::loan(LoanEventKind, &LoanDetails), Webhook { endpoint: String, event: WebhookEvent }, WebhookSender::new(&[WebhookEndpoint], connect_timeout, timeout) -> anyhow::Result<Self>, WebhookSender::from_config(&JobsConfig) -> anyhow::Result<Option<Self>>, deliver(&self, endpoint: &str, &WebhookEvent) -> Result<(), JobError>, sign(key: &[u8], id: &str, timestamp: i64, body: &[u8]) -> Result<String, JobError>}`.

- [ ] **Step 1: Write the failing tests**

Unit tests at the bottom of `apps/web/src/jobs/webhook.rs` (create the file with just this module first):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_matches_the_standard_webhooks_reference_vector() {
        let key = STANDARD.decode("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap();
        let sig = sign(&key, "msg_p5jXN8AQM9LWM0D4loKWxJek", 1_614_265_330, br#"{"test": 2432232314}"#).unwrap();
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
    }

    #[test]
    fn statuses_map_to_outcomes() {
        let ok = |s: u16| outcome(StatusCode::from_u16(s).unwrap(), None, "e");
        assert!(ok(200).is_ok() && ok(204).is_ok());
        for retry in [408, 429, 500, 502, 503] {
            assert!(matches!(ok(retry), Err(JobError::Retry(_))), "{retry}");
        }
        for permanent in [301, 302, 400, 401, 404, 410, 422] {
            assert!(matches!(ok(permanent), Err(JobError::Permanent(_))), "{permanent}");
        }
        let later = outcome(StatusCode::TOO_MANY_REQUESTS, Some(Duration::from_secs(7)), "e");
        assert!(matches!(later, Err(JobError::RetryAfter(d, _)) if d == Duration::from_secs(7)));
    }

    #[test]
    fn retry_after_takes_seconds_and_caps_them() {
        let h = |v: &str| {
            let mut m = reqwest::header::HeaderMap::new();
            m.insert(reqwest::header::RETRY_AFTER, v.parse().unwrap());
            retry_after(&m)
        };
        assert_eq!(h("7"), Some(Duration::from_secs(7)));
        assert_eq!(h("999999"), Some(MAX_RETRY_AFTER));
        assert_eq!(h("Wed, 21 Oct 2015 07:28:00 GMT"), None, "dates fall back to the normal backoff");
    }
}
```

Create `apps/web/tests/jobs_webhook.rs`:

```rust
//! `webhook.v1` against an in-test receiver. No database: the job context gets a
//! lazy pool that never connects.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{TimeZone, Utc};
use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::jobs::webhook::{Webhook, WebhookEvent, WebhookSender, sign};
use library_web::jobs_config::{Secret, WebhookEndpoint};
use serde_json::{Value, json};

const KEY: [u8; 32] = [7; 32];

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>,
    replies: Arc<Mutex<VecDeque<(u16, Vec<(&'static str, String)>)>>>,
}

async fn hit(State(r): State<Receiver>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    r.hits.lock().unwrap().push((uri.path().to_owned(), headers, body));
    let (status, headers) = r.replies.lock().unwrap().pop_front().unwrap_or((204, Vec::new()));
    let mut response = StatusCode::from_u16(status).unwrap().into_response();
    for (name, value) in headers {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}

/// Start a receiver; returns it and its base URL.
async fn receiver() -> (Receiver, String) {
    let r = Receiver::default();
    let app = Router::new().fallback(hit).with_state(r.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, base)
}

fn endpoint(url: &str) -> WebhookEndpoint {
    WebhookEndpoint { url: url.to_owned(), secret: Secret::new(format!("whsec_{}", STANDARD.encode(KEY))) }
}

fn sender(urls: &[&str], timeout: Duration) -> WebhookSender {
    let endpoints: Vec<_> = urls.iter().map(|u| endpoint(u)).collect();
    WebhookSender::new(&endpoints, Duration::from_secs(2), timeout).unwrap()
}

fn event() -> WebhookEvent {
    WebhookEvent {
        id: "loan.7.borrowed".into(),
        kind: "loan.borrowed".into(),
        occurred_at: Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        data: json!({ "loan": { "id": 7 } }),
    }
}

#[tokio::test]
async fn a_delivered_event_is_signed_and_verifiable() {
    let (r, base) = receiver().await;
    let url = format!("{base}/hook");
    sender(&[url.as_str()], Duration::from_secs(5)).deliver(&url, &event()).await.unwrap();
    let hits = r.hits.lock().unwrap().clone();
    let (path, headers, body) = &hits[0];
    assert_eq!(path, "/hook");
    assert_eq!(headers["webhook-id"], "loan.7.borrowed");
    let ts: i64 = headers["webhook-timestamp"].to_str().unwrap().parse().unwrap();
    assert_eq!(headers["webhook-signature"].to_str().unwrap(), sign(&KEY, "loan.7.borrowed", ts, body).unwrap());
    let json: Value = serde_json::from_slice(body).unwrap();
    assert_eq!((json["id"].as_str(), json["type"].as_str()), (Some("loan.7.borrowed"), Some("loan.borrowed")));
    assert_eq!(json["occurred_at"], "2027-01-15T08:00:00Z");
}

#[tokio::test]
async fn too_many_requests_honours_retry_after() {
    let (r, base) = receiver().await;
    r.replies.lock().unwrap().push_back((429, vec![("retry-after", "7".into())]));
    let err = sender(&[base.as_str()], Duration::from_secs(5)).deliver(&base, &event()).await.unwrap_err();
    assert!(matches!(err, JobError::RetryAfter(d, _) if d == Duration::from_secs(7)), "{err:?}");
}

#[tokio::test]
async fn server_errors_retry_and_client_errors_are_permanent() {
    let (r, base) = receiver().await;
    let s = sender(&[base.as_str()], Duration::from_secs(5));
    for (status, retry) in [(500, true), (408, true), (404, false), (401, false)] {
        r.replies.lock().unwrap().push_back((status, Vec::new()));
        let err = s.deliver(&base, &event()).await.unwrap_err();
        assert_eq!(matches!(err, JobError::Retry(_)), retry, "{status}: {err:?}");
        assert_eq!(matches!(err, JobError::Permanent(_)), !retry, "{status}: {err:?}");
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (r, base) = receiver().await;
    let url = format!("{base}/hook");
    r.replies.lock().unwrap().push_back((302, vec![("location", format!("{base}/elsewhere"))]));
    let err = sender(&[url.as_str()], Duration::from_secs(5)).deliver(&url, &event()).await.unwrap_err();
    assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
    let paths: Vec<String> = r.hits.lock().unwrap().iter().map(|h| h.0.clone()).collect();
    assert_eq!(paths, ["/hook"]);
}

#[tokio::test]
async fn hanging_endpoint_times_out_as_retry() {
    // Accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await { held.push(socket); }
    });
    let start = Instant::now();
    let err = sender(&[url.as_str()], Duration::from_secs(1)).deliver(&url, &event()).await.unwrap_err();
    assert!(matches!(err, JobError::Retry(_)), "{err:?}");
    assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
}

#[tokio::test]
async fn an_endpoint_removed_from_config_is_permanent() {
    let err = sender(&["http://127.0.0.1:9/a"], Duration::from_secs(1)).deliver("http://127.0.0.1:9/b", &event()).await.unwrap_err();
    assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn perform_uses_the_provided_sender() {
    let (r, base) = receiver().await;
    let mut deps = Deps::new();
    deps.insert(Arc::new(sender(&[base.as_str()], Duration::from_secs(5))));
    // Never connects: the webhook job does not touch the database.
    let pool = sqlx::postgres::PgPoolOptions::new().connect_lazy("postgres://unused@127.0.0.1:1/unused").unwrap(); // local only
    let job = Webhook { endpoint: base.clone(), event: event() };
    job.perform(JobContext::detached(pool, 1, 1, Webhook::KIND, deps)).await.unwrap();
    assert_eq!(r.hits.lock().unwrap().len(), 1);
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p library-web --features postgres --test jobs_webhook`
Expected: compile errors (`jobs::webhook` not found).

- [ ] **Step 3: Implement `webhook.rs`**

Above the tests module:

```rust
//! `webhook.v1`: POST a loan event to an operator-configured endpoint, signed per
//! the Standard Webhooks spec (spec §15.3).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use library_jobs::{Job, JobContext, JobError, JobOptions};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;

use super::events::{LoanEventKind, event_id};
use crate::jobs_config::{JobsConfig, WebhookEndpoint, decode_secret};
use crate::loans::LoanDetails;

/// Response bodies are read up to this size (so the connection can be reused), then dropped.
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
/// The longest `Retry-After` honoured.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3600);

/// The JSON body: `{id, type, occurred_at, data}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WebhookEvent {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub occurred_at: DateTime<Utc>,
    pub data: Value,
}

impl WebhookEvent {
    /// Captured at enqueue: an event is a fact at a point in time, unlike the email.
    pub fn loan(kind: LoanEventKind, loan: &LoanDetails) -> Self {
        let occurred_at = match kind {
            LoanEventKind::Borrowed => loan.borrowed_at,
            LoanEventKind::Returned => loan.returned_at.unwrap_or(loan.borrowed_at),
        };
        Self {
            id: event_id(loan.id, kind),
            kind: format!("loan.{}", kind.as_str()),
            occurred_at,
            data: json!({ "loan": {
                "id": loan.id,
                "book_id": loan.book_id,
                "book_title": loan.book_title,
                "member_id": loan.member_id,
                "member_name": loan.member_name,
                "borrowed_at": loan.borrowed_at,
                "due_at": loan.due_at,
                "returned_at": loan.returned_at,
            }}),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Webhook {
    /// Must be one of `JOBS_WEBHOOK_ENDPOINTS` when the job runs.
    pub endpoint: String,
    pub event: WebhookEvent,
}

impl Job for Webhook {
    const KIND: &'static str = "webhook.v1";
    const QUEUE: &'static str = "webhooks";

    fn options() -> JobOptions {
        JobOptions { deadline: Duration::from_secs(60), ..JobOptions::default() }
    }

    fn unique_key(&self) -> Option<String> {
        Some(format!("{} {}", self.event.id, self.endpoint))
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        ctx.require::<WebhookSender>()?.deliver(&self.endpoint, &self.event).await
    }
}

/// Standard Webhooks: `v1,` + base64(HMAC-SHA256(key, "{id}.{timestamp}.{body}")).
pub fn sign(key: &[u8], id: &str, timestamp: i64, body: &[u8]) -> Result<String, JobError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| JobError::permanent("the webhook key was refused by HMAC"))?;
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes())))
}

/// One HTTP client and the decoded keys, shared by every webhook job of a process.
pub struct WebhookSender {
    client: reqwest::Client,
    keys: HashMap<String, Vec<u8>>,
}

impl WebhookSender {
    pub fn new(endpoints: &[WebhookEndpoint], connect_timeout: Duration, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(connect_timeout)
            .timeout(timeout)
            .user_agent(concat!("library-web/", env!("CARGO_PKG_VERSION"), " webhooks"))
            .build()
            .context("building the webhook HTTP client")?;
        let keys = endpoints
            .iter()
            .map(|e| Ok((e.url.clone(), decode_secret(e.secret.expose()).with_context(|| format!("secret of {}", e.url))?)))
            .collect::<anyhow::Result<_>>()?;
        Ok(Self { client, keys })
    }

    /// `None` when no endpoint is configured: the webhook kind is then not registered.
    pub fn from_config(jobs: &JobsConfig) -> anyhow::Result<Option<Self>> {
        if jobs.webhooks.is_empty() {
            return Ok(None);
        }
        Self::new(&jobs.webhooks, Duration::from_secs(5), Duration::from_secs(15)).map(Some)
    }

    pub async fn deliver(&self, endpoint: &str, event: &WebhookEvent) -> Result<(), JobError> {
        let Some(key) = self.keys.get(endpoint) else {
            return Err(JobError::permanent(format!("endpoint {endpoint} is no longer configured")));
        };
        let body = serde_json::to_vec(event).map_err(JobError::permanent)?;
        // The receiver compares this with its own clock to refuse replays, so it is
        // the send time; it is never stored or compared by us.
        let timestamp = Utc::now().timestamp();
        let signature = sign(key, &event.id, timestamp, &body)?;
        let response = self
            .client
            .post(endpoint)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("webhook-id", &event.id)
            .header("webhook-timestamp", timestamp.to_string())
            .header("webhook-signature", signature)
            .body(body)
            .send()
            .await
            .map_err(|err| JobError::retry(format!("POST {endpoint}: {err}")))?;
        let status = response.status();
        let retry_after = retry_after(response.headers());
        drain(response).await;
        outcome(status, retry_after, endpoint)
    }
}

async fn drain(mut response: reqwest::Response) {
    let mut read = 0;
    while read < MAX_RESPONSE_BYTES {
        match response.chunk().await {
            Ok(Some(chunk)) => read += chunk.len(),
            _ => break,
        }
    }
}

/// `Retry-After` in seconds; HTTP dates fall back to the normal backoff.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let secs: u64 = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

/// 2xx → done; 408, 429 and 5xx → retry; redirects and other 4xx → permanent.
fn outcome(status: StatusCode, retry_after: Option<Duration>, endpoint: &str) -> Result<(), JobError> {
    if status.is_success() {
        return Ok(());
    }
    let what = format!("POST {endpoint}: HTTP {}", status.as_u16());
    if status == StatusCode::REQUEST_TIMEOUT || status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Err(match retry_after {
            Some(delay) => JobError::RetryAfter(delay, what),
            None => JobError::Retry(what),
        });
    }
    if status.is_redirection() {
        return Err(JobError::Permanent(format!("{what} (redirects are not followed)")));
    }
    Err(JobError::Permanent(what))
}
```

In `jobs/mod.rs`: add `pub mod webhook;` and extend `registry`, before `Ok(registry)`:

```rust
    if let Some(sender) = webhook::WebhookSender::from_config(jobs)? {
        registry = registry.register::<webhook::Webhook>().provide(Arc::new(sender));
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p library-web --features postgres --lib webhook && cargo test -p library-web --features postgres --test jobs_webhook`
Expected: 3 unit + 7 integration PASS.

- [ ] **Step 5: Commit**

```bash
cargo clippy -p library-web --all-targets --features postgres -- -D warnings && cargo fmt
git add apps/web
git commit -m "web: webhook.v1 with Standard Webhooks signatures, no redirects, Retry-After, bounded timeouts"
```

---

### Task 18: Enqueue on borrow and return (the outbox in the loan transactions)

**Files:**
- Modify: `apps/web/src/jobs/events.rs`, `apps/web/src/loans/postgres.rs`, `apps/web/src/state.rs`, `apps/web/tests/common/mod.rs`
- Create: `apps/web/tests/jobs_events.rs`

**Interfaces:**
- Consumes: `Enqueuer`, `EnqueueError` (Task 3); `LoanEmail` (Task 16); `Webhook`, `WebhookEvent` (Task 17); `JobsConfig` (Task 15); `jobs::registry`, `jobs::worker_config` (Tasks 15–17).
- Produces: `jobs::events::LoanEvents::from_config(&JobsConfig) -> Option<LoanEvents>`, `LoanEvents::record(&self, &mut PgConnection, LoanEventKind, &LoanDetails) -> Result<(), EnqueueError>`; `PostgresLoanRepository::with_events(self, Option<LoanEvents>) -> Self`; test helper `TestApp::with_jobs(db, JobsConfig)`.

- [ ] **Step 1: Write the failing tests**

In `apps/web/tests/common/mod.rs`, inside `impl TestApp`:

```rust
    /// Like [`new`](Self::new) but with background jobs configured (Postgres only).
    #[cfg(feature = "postgres")]
    pub fn with_jobs(db: impl Into<Database>, jobs: library_web::jobs_config::JobsConfig) -> Self {
        Self::with_config(db.into(), Config { jobs, ..test_config() })
    }
```

Create `apps/web/tests/jobs_events.rs`:

```rust
//! Borrow and return enqueue their jobs in the same transaction (spec §15.1).
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use common::TestApp;
use library_web::db::Database;
use library_web::jobs_config::{JobsConfig, Secret, WebhookEndpoint};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

// Test key, local only.
const SECRET: &str = "whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn jobs_on(smtp: bool, endpoints: &[&str]) -> JobsConfig {
    JobsConfig {
        enabled: true,
        notify: false,
        smtp_url: smtp.then(|| Secret::new("smtp://127.0.0.1:1")),
        webhooks: endpoints.iter().map(|u| WebhookEndpoint { url: (*u).to_owned(), secret: Secret::new(SECRET) }).collect(),
        ..JobsConfig::default()
    }
}

/// The app's schema is migrated by `on_postgres`; the jobs schema is added here.
async fn with_jobs_schema(db: &Database) -> PgPool {
    let Database::Postgres(pool) = db else { panic!("postgres only") };
    library_web::jobs::migrate(pool).await.unwrap();
    pool.clone()
}

async fn setup(app: &TestApp, copies: i64) -> (String, i64, i64) {
    let cookie = app.login("librarian", "correct horse battery").await;
    let (s, _, book) = app.post_json("/api/books", &json!({ "title": "Dune", "author": "Herbert", "total_copies": copies }), Some(&cookie)).await;
    assert_eq!(s, StatusCode::CREATED, "{book}");
    let (s, _, member) = app.post_json("/api/members", &json!({ "name": "Ann", "email": "ann@example.com" }), Some(&cookie)).await;
    assert_eq!(s, StatusCode::CREATED, "{member}");
    (cookie, book["id"].as_i64().unwrap(), member["id"].as_i64().unwrap())
}

async fn jobs(pool: &PgPool) -> Vec<(String, String, Value)> {
    sqlx::query_as("SELECT kind, queue, args FROM jobs ORDER BY id").fetch_all(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn borrow_and_return_enqueue_one_email_and_one_webhook_per_endpoint() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::with_jobs(db, jobs_on(true, &["https://a.example/h", "https://b.example/h"]));
        let (cookie, book, member) = setup(&app, 1).await;
        let (s, _, loan) = app.post_json("/api/loans", &json!({ "book_id": book, "member_id": member }), Some(&cookie)).await;
        assert_eq!(s, StatusCode::CREATED);
        let id = loan["id"].as_i64().unwrap();

        let rows = jobs(&pool).await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!((rows[0].0.as_str(), rows[0].1.as_str()), ("loan_email.v1", "email"));
        assert_eq!(rows[0].2, json!({ "loan_id": id, "event": "borrowed" }));
        let mut endpoints: Vec<&str> = rows[1..].iter().map(|r| r.2["endpoint"].as_str().unwrap()).collect();
        endpoints.sort_unstable();
        assert_eq!(endpoints, ["https://a.example/h", "https://b.example/h"]);
        for r in &rows[1..] {
            assert_eq!((r.0.as_str(), r.1.as_str()), ("webhook.v1", "webhooks"));
            assert_eq!(r.2["event"]["id"], format!("loan.{id}.borrowed"));
            assert_eq!(r.2["event"]["type"], "loan.borrowed");
            assert_eq!(r.2["event"]["data"]["loan"]["book_title"], "Dune");
        }

        let (s, _, _) = app.post_json(&format!("/api/loans/{id}/return"), &json!({}), Some(&cookie)).await;
        assert_eq!(s, StatusCode::OK);
        let rows = jobs(&pool).await;
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[3].2, json!({ "loan_id": id, "event": "returned" }));
        assert!(rows[4..].iter().all(|r| r.2["event"]["type"] == "loan.returned"));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_borrow_enqueues_nothing() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::with_jobs(db, jobs_on(true, &["https://a.example/h"]));
        let (cookie, book, member) = setup(&app, 1).await;
        let body = json!({ "book_id": book, "member_id": member });
        assert_eq!(app.post_json("/api/loans", &body, Some(&cookie)).await.0, StatusCode::CREATED);
        assert_eq!(app.post_json("/api/loans", &body, Some(&cookie)).await.0, StatusCode::CONFLICT);
        assert_eq!(jobs(&pool).await.len(), 2, "only the first borrow's email and webhook");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn with_jobs_disabled_nothing_is_enqueued() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::new(db);
        let (cookie, book, member) = setup(&app, 1).await;
        let (s, _, _) = app.post_json("/api/loans", &json!({ "book_id": book, "member_id": member }), Some(&cookie)).await;
        assert_eq!(s, StatusCode::CREATED);
        assert!(jobs(&pool).await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_enqueue_rolls_the_borrow_back() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::with_jobs(db, jobs_on(true, &[]));
        let (cookie, book, member) = setup(&app, 1).await;
        sqlx::query("DROP TABLE jobs CASCADE").execute(&pool).await.unwrap();
        let (s, _, _) = app.post_json("/api/loans", &json!({ "book_id": book, "member_id": member }), Some(&cookie)).await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        let loans: i64 = sqlx::query_scalar("SELECT count(*) FROM loans").fetch_one(&pool).await.unwrap();
        assert_eq!(loans, 0, "no loan without its jobs");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_delivers_the_borrow_webhook() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let receiver = {
            let seen = seen.clone();
            Router::new().fallback(move |headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(headers["webhook-id"].to_str().unwrap().to_owned());
                    StatusCode::NO_CONTENT
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

        let config = jobs_on(false, &[url.as_str()]);
        let app = TestApp::with_jobs(db, config.clone());
        let mut worker_config = library_web::jobs::worker_config(&config, "test").unwrap();
        worker_config.poll = Duration::from_millis(100);
        let worker = library_jobs::Worker::new(pool.clone(), library_web::jobs::registry(&config).unwrap(), worker_config).unwrap();
        let stop = CancellationToken::new();
        let handle = tokio::spawn(worker.run(stop.clone()));

        let (cookie, book, member) = setup(&app, 1).await;
        let (_, _, loan) = app.post_json("/api/loans", &json!({ "book_id": book, "member_id": member }), Some(&cookie)).await;
        let want = format!("loan.{}.borrowed", loan["id"]);
        let start = Instant::now();
        while !seen.lock().unwrap().contains(&want) {
            assert!(start.elapsed() < Duration::from_secs(10), "webhook not delivered in 10 s");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.cancel();
        handle.await.unwrap().unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE kind = 'webhook.v1'").fetch_one(&pool).await.unwrap();
        assert_eq!(state, "succeeded");
    })
    .await;
}
```

- [ ] **Step 2: Run to see them fail**

Run: `cargo test -p library-web --features postgres --test jobs_events`
Expected: compile error (`TestApp::with_jobs` exists after the edit above, but jobs are never enqueued: the first test fails with `rows.len() == 0`). If it compiles, the run shows 4 failures (the disabled case passes).

- [ ] **Step 3: Implement `LoanEvents`**

Append to `apps/web/src/jobs/events.rs`:

```rust
use library_jobs::{EnqueueError, Enqueuer};
use sqlx::PgConnection;

use super::loan_email::LoanEmail;
use super::webhook::{Webhook, WebhookEvent};
use crate::jobs_config::JobsConfig;
use crate::loans::LoanDetails;

/// Enqueues the jobs of a loan event. The repository calls it inside its
/// transaction, just before commit: the jobs exist exactly when the loan change
/// does (the outbox, spec §6.2).
#[derive(Debug, Clone)]
pub struct LoanEvents {
    enqueuer: Enqueuer,
    email: bool,
    endpoints: Vec<String>,
}

impl LoanEvents {
    /// `None` with jobs disabled. Emails only with an SMTP URL, webhooks one per
    /// configured endpoint (they fail and retry independently).
    pub fn from_config(jobs: &JobsConfig) -> Option<Self> {
        jobs.enabled.then(|| Self {
            enqueuer: Enqueuer::new(jobs.notify),
            email: jobs.smtp_url.is_some(),
            endpoints: jobs.webhooks.iter().map(|e| e.url.clone()).collect(),
        })
    }

    /// `conn` is the caller's open transaction.
    pub async fn record(&self, conn: &mut PgConnection, kind: LoanEventKind, loan: &LoanDetails) -> Result<(), EnqueueError> {
        if self.email {
            self.enqueuer.enqueue(&mut *conn, &LoanEmail { loan_id: loan.id, event: kind }).await?;
        }
        if !self.endpoints.is_empty() {
            let event = WebhookEvent::loan(kind, loan);
            for endpoint in &self.endpoints {
                let job = Webhook { endpoint: endpoint.clone(), event: event.clone() };
                self.enqueuer.enqueue(&mut *conn, &job).await?;
            }
        }
        Ok(())
    }
}
```

(Move the new `use` lines to the top of the file, next to `use serde::…`.)

- [ ] **Step 4: Call it from the repository**

In `apps/web/src/loans/postgres.rs`:

```rust
use crate::jobs::events::{LoanEventKind, LoanEvents};
```

Add the field and builder:

```rust
#[derive(Debug, Clone)]
pub struct PostgresLoanRepository {
    pool: PgPool,
    counts: Arc<CountCache>,
    /// Jobs to enqueue on borrow and return; `None` with jobs disabled.
    events: Option<LoanEvents>,
}
```

`new` sets `events: None`, and:

```rust
    /// Enqueue loan-event jobs in the borrow and return transactions (spec §15.1).
    pub fn with_events(mut self, events: Option<LoanEvents>) -> Self {
        self.events = events;
        self
    }
```

In `create_checked`, between `let created = fetch_loan(…)…?;` and `tx.commit().await?;`:

```rust
        if let Some(events) = &self.events {
            events
                .record(&mut tx, LoanEventKind::Borrowed, &created)
                .await
                .map_err(|err| RepoError::Unexpected(err.into()))?;
        }
```

In `mark_returned`, between `let returned = fetch_loan(…)…?;` and `tx.commit().await?;`:

```rust
        if let Some(events) = &self.events {
            events
                .record(&mut tx, LoanEventKind::Returned, &returned)
                .await
                .map_err(|err| RepoError::Unexpected(err.into()))?;
        }
```

In `apps/web/src/state.rs`, the Postgres loan repository line becomes:

```rust
                Arc::new(
                    PostgresLoanRepository::new(pool.clone())
                        .with_count_cache(counts.clone())
                        .with_events(crate::jobs::events::LoanEvents::from_config(&config.jobs)),
                ),
```

- [ ] **Step 5: Run the tests**

Run:
```bash
cargo test -p library-web --features postgres --test jobs_events
cargo test -p library-web --features postgres
cargo test -p library-web
```
Expected: 5 PASS in `jobs_events`; every other test still PASS on both backends (with jobs disabled the repository behaves exactly as before).

- [ ] **Step 6: Commit**

```bash
cargo clippy -p library-web --all-targets --features postgres -- -D warnings && cargo clippy -p library-web --all-targets -- -D warnings && cargo fmt
git add apps/web
git commit -m "web: borrow and return enqueue loan_email and webhook jobs inside their transactions"
```

---

### Task 19: Deployment, CI, docs, spec deviations, soak and acceptance

**Files:**
- Modify: `deploy/compose/local/loadtest.web.yml`, `deploy/compose/local/loadtest.web-pg.yml`, `.github/workflows/ci.yml`, `Makefile`, `docs/RUNBOOK.md`, `docs/ARCHITECTURE.md`, `docs/superpowers/specs/2026-10-14-background-jobs-design.md`, `.env.example`
- Create: `docs/JOBS.md`, `docs/decisions/0007-own-job-queue.md`

- [ ] **Step 1: Compose**

In `deploy/compose/local/loadtest.web.yml`, add to the `web` service's `environment` (after `DATABASE_MAX_CONNECTIONS:`):

```yaml
      # Background jobs (Postgres only; see docs/JOBS.md). Off unless the runner's
      # environment sets them; with the `jobs` profile the worker below runs too.
      JOBS_ENABLED: ${JOBS_ENABLED:-false}
      JOBS_SMTP_URL: ${JOBS_SMTP_URL:-}
```

In `deploy/compose/local/loadtest.web-pg.yml`, add under `services:` (after `web:`):

```yaml
  # Background jobs: `COMPOSE_PROFILES=jobs JOBS_ENABLED=true JOBS_SMTP_URL=smtp://mailpit:1025`.
  # For functional checks (the contract test with jobs on); its memory is outside
  # the box's 4096 MiB split, so measured rounds leave the profile off.
  worker:
    profiles: [jobs]
    image: ${WEB_IMAGE:-sqlite-production/web:lt}
    command: ["worker"]
    environment:
      DATABASE_URL: postgres://library:library@postgres:5432/library # local only
      JOBS_ENABLED: "true"
      JOBS_SMTP_URL: ${JOBS_SMTP_URL:-}
      JOBS_METRICS_ADDR: 0.0.0.0:9300
      RUST_LOG: info
    cpuset: "${NODE_CPUS:-6,7}"
    mem_limit: 256m
    stop_grace_period: 40s
    healthcheck:
      test: ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:9300/readyz"]
      interval: 10s
      timeout: 3s
      retries: 3
      start_period: 10s
      start_interval: 1s
    depends_on:
      postgres:
        condition: service_healthy
    restart: unless-stopped

  mailpit:
    profiles: [jobs]
    image: axllent/mailpit:v1.31.2
    mem_limit: 128m
    restart: unless-stopped
```

The image's `HEALTHCHECK` probes `/api/health` on port 4000; the service-level `healthcheck` above replaces it for the worker.

Run: `docker compose -f deploy/compose/local/loadtest.web.yml -f deploy/compose/local/loadtest.web-pg.yml --profile jobs config >/dev/null`
Expected: exit 0 (the files merge and validate).

- [ ] **Step 2: CI and Makefile**

In `.github/workflows/ci.yml`, job `rust`: add a second service and its variables, and the web Postgres steps:

```yaml
      mailpit:
        image: axllent/mailpit:v1.31.2
        env:
          MP_ENABLE_CHAOS: "true"
        ports: ["55025:1025", "58025:8025"]
```

```yaml
    env:
      TEST_POSTGRES_URL: postgres://postgres:test@127.0.0.1:55432/postgres
      TEST_SMTP_URL: smtp://127.0.0.1:55025
      TEST_MAILPIT_URL: http://127.0.0.1:58025
```

after `- run: cargo test --workspace`:

```yaml
      - run: cargo clippy -p library-web --all-targets --features postgres -- -D warnings
      - run: cargo test -p library-web --features postgres
```

In the `Makefile`, `test` becomes:

```make
test: ## Rust tests, default build and web --features postgres; starts a throwaway Postgres and Mailpit (Docker)
	url=$$(./scripts/test-postgres.sh) && eval "$$(./scripts/test-mailpit.sh)" && \
	TEST_POSTGRES_URL=$$url cargo test --workspace && \
	TEST_POSTGRES_URL=$$url cargo test -p library-web --features postgres
```

In `.env.example`, add:

```bash
# Background jobs (Postgres only, docs/JOBS.md). Off by default.
# JOBS_ENABLED=true
# JOBS_SMTP_URL=smtp://127.0.0.1:55025
# JOBS_WEBHOOK_ENDPOINTS=https://example.test/hook
# JOBS_WEBHOOK_SECRET_1=whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=  # local only
```

Run: `make test`
Expected: PASS.

- [ ] **Step 3: `docs/JOBS.md`**

```markdown
# Background jobs

Postgres only. The queue is the `library-jobs` crate (`crates/jobs`); the app's
jobs live in `apps/web/src/jobs/`. Design: `docs/superpowers/specs/2026-10-14-background-jobs-design.md`.
Why our own crate: [decision 0007](decisions/0007-own-job-queue.md).

## Concepts

- **Job**: a row in `jobs` with a `kind` (`loan_email.v1`), a `queue` (`email`) and JSON `args`.
  The kind's version suffix changes whenever its arguments change shape.
- **Outbox**: a job is inserted with the caller's connection, inside the caller's transaction.
  It exists exactly when the data change commits. Borrow and return do this (`loans/postgres.rs`).
- **Worker process**: `library-web worker`. One claim loop per queue (`JOBS_QUEUES`, `name:slots[:max_running]`),
  one heartbeat task, one LISTEN connection, and a candidate for maintenance leader.
- **Lease**: a running job belongs to one process until `locked_until` (60 s by default per kind). The heartbeat
  (every 20 s) renews the leases of all running jobs in one statement. A process that stops beating loses its jobs;
  the leader re-queues them as a crash (`crashes + 1`, cause `lease_expired` or `process_lost`).
- **Fencing**: every outcome write checks `locked_by` and `attempt`. A process that lost a job can no longer change it.
- **Leader**: one process holds an advisory lock and runs the reaper (5 s), pruner (30 s), recurring ticks,
  retention (succeeded 24 h, failed 14 d) and the queue gauges.

## Guarantees

- At least once. A handler can run again after a crash between its side effect and the completion write, so
  handlers must be idempotent or de-duplicate downstream (the email's `Message-ID` and the webhook's `webhook-id`
  are stable across attempts).
- A job is never lost: it ends `succeeded`, `failed` (attempts or crashes used up, or a permanent error) or `cancelled`.
- Unique keys: while a job with the same kind and key is waiting or running, enqueue returns `Duplicate(id)` and
  creates nothing, without failing the caller's transaction.
- All times are database time.

## Writing a job

1. A struct with `Serialize + Deserialize`, and `impl Job` with `KIND` (`name.v1`), `QUEUE`, optional `options()`,
   `unique_key()`, `concurrency_key()`, and `async fn perform(self, ctx: JobContext) -> Result<(), JobError>`.
2. Errors: `JobError::Retry` (backoff 5 s doubling to 1 h, full jitter), `RetryAfter(delay)`, `Permanent` (no retry),
   `Cancelled`. A panic counts as a retryable error.
3. Long jobs check `ctx.cancelled()` and report `ctx.set_progress(json)`.
4. Register it in `jobs::registry()` and give it what it needs with `.provide(Arc::new(..))`; the handler takes it
   with `ctx.require::<T>()`.
5. Enqueue with `Enqueuer::enqueue(&mut tx, &job)` inside your transaction.

## Configuration

| Variable | Default | Meaning |
|---|---|---|
| `JOBS_ENABLED` | `false` | Enqueue, migrate the jobs schema, allow `worker` |
| `JOBS_EMBEDDED` | `false` | Also run a worker inside `serve` |
| `JOBS_QUEUES` | `email:4,webhooks:8` | Queues and handler slots of a worker |
| `JOBS_POLL_MS` | `1000` | Fallback poll; NOTIFY wakes workers sooner |
| `JOBS_NOTIFY` | `true` | NOTIFY on enqueue, LISTEN in workers |
| `JOBS_DB_POOL` | `16` | The worker's own pool |
| `JOBS_SHUTDOWN_TIMEOUT_S` | `25` | Wait for running jobs on SIGTERM, then cancel and release them |
| `JOBS_KEEP_SUCCEEDED_H` / `JOBS_KEEP_FAILED_H` | `24` / `336` | Retention |
| `JOBS_METRICS_ADDR` | `0.0.0.0:9300` | `/metrics`, `/healthz`, `/readyz` of `worker` |
| `JOBS_SMTP_URL` | unset | `smtp://…` or `smtps://…`; unset: no emails are enqueued or sent |
| `JOBS_SMTP_TIMEOUT_S` | `30` | Per SMTP connection and reply |
| `JOBS_EMAIL_FROM` / `JOBS_EMAIL_DOMAIN` | `library@localhost` / `library.local` | Sender and `Message-ID` domain |
| `JOBS_WEBHOOK_ENDPOINTS` | unset | Comma-separated URLs (max 10) |
| `JOBS_WEBHOOK_SECRET_<n>` | – | `whsec_<base64 key>` for the n-th endpoint (1-based) |

Secrets are never logged and never stored in job arguments.

## Operating

- `library-web jobs status`: per queue available, scheduled, running, failed in 24 h, oldest wait; processes;
  kinds no live process can run.
- `jobs list --state failed`, `jobs show <id>`, `jobs retry <id>`, `jobs retry --failed --kind webhook.v1 --since 2h`,
  `jobs discard <id>`, `jobs cancel <id>`, `jobs pause <queue> --reason …`, `jobs resume <queue>`,
  `jobs processes`, `jobs prune --dry-run`.
- Metrics (Prometheus): `jobs_completed_total{kind,outcome}`, `jobs_duration_seconds`, `jobs_wait_seconds`,
  `jobs_running`, `jobs_slots`, and from the leader `jobs_available`, `jobs_scheduled`,
  `jobs_oldest_available_seconds`, `jobs_failed_recent`, `jobs_processes_live`, `jobs_leader`.
- Health: `/readyz` is 503 when the process fenced itself, its heartbeat is older than three periods, or the
  database does not answer.
- Throughput on the 2-core box: <fill in the number measured in Task 14 Step 6> no-op jobs/s with one worker.
```

Replace the `<fill in …>` line with the number Task 14 measured before committing.

- [ ] **Step 4: Runbook, architecture, decision record**

Append to `docs/RUNBOOK.md`:

```markdown
## Background jobs

See `docs/JOBS.md` for concepts. All commands are `library-web jobs …` with the worker's environment.

- **A queue is not moving**: `jobs status`. Oldest wait growing with `running` at the slot count: the handlers are
  slow or stuck; `jobs list --state running --queue q` and `jobs show <id>` (progress, attempt). A queue marked
  paused: `jobs resume q`. Kinds listed under "no live process can run": a worker with that kind (and its settings,
  e.g. `JOBS_SMTP_URL`) is not running.
- **Failed jobs**: `jobs list --state failed`, read `jobs show <id>` errors. After fixing the cause:
  `jobs retry --failed --kind <kind> --since 6h`.
- **Poison job** (fails with `lease_expired`/`process_lost` until `max_crashes`): it killed or hung its worker.
  Leave it failed, fix the handler, then `jobs retry <id>`.
- **SMTP outage**: emails retry with backoff for about 10 attempts. If the outage was longer, they end failed;
  replay with `jobs retry --failed --kind loan_email.v1 --since <outage length>`. The stable Message-ID lets
  mail systems drop duplicates.
- **Pause a queue** (a receiver asks us to stop): `jobs pause webhooks --reason "…"`; running jobs finish.
- **Adding a webhook endpoint**: append its URL to `JOBS_WEBHOOK_ENDPOINTS` and set
  `JOBS_WEBHOOK_SECRET_<its position>` on both `serve` and `worker`, then restart both. Never reorder existing
  entries: the secret is matched by position. Removing an endpoint makes its waiting jobs fail as permanent.
- **A worker exits with "fenced itself off"**: its registry row was pruned (it missed heartbeats for 3 minutes,
  e.g. the VM was suspended). The supervisor restarts it; its jobs were re-queued.
```

In `docs/ARCHITECTURE.md` module map, add rows for `crates/jobs` (the generic queue: schema, enqueue, worker runtime, leader, metrics, ops queries) and `apps/web/src/jobs` (the app's job kinds, enqueue points, `worker` and `jobs` subcommands), in the style of the existing rows.

Create `docs/decisions/0007-own-job-queue.md`:

```markdown
# 0007: Our own Postgres job queue

Date: 2026-10-14. Status: accepted.

## Context

Borrowing and returning a book must notify the member (email) and outside systems (webhooks) reliably,
without making the request slower or failing it when SMTP or a receiver is down. More background work
(reports, imports) follows. The notification must exist if and only if the loan change commits.

## Decision

A transactional outbox and job queue in the same Postgres database, as our own crate `library-jobs`,
with per-job leases renewed by one heartbeat per process, fenced outcome writes, a process registry and an
elected maintenance leader, run by a separate `library-web worker` process.

## Alternatives

- **apalis-postgres 1.0**: accepts the caller's transaction, but heartbeats are per worker rather than per job,
  there is no database-side backoff, no progress, no cross-process cancel, a duplicate key fails the whole insert,
  and it was days old.
- **fang**: no outbox (its own pool), a non-atomic claim, no crash recovery.
- **BullMQ (Rust client)**: Redis only, so no outbox with our data; a lost lock does not stop the job.
- **graphile_worker**: Postgres and transactional enqueue, but not examined deeply enough to depend on.
- **Solid Queue's model**, copied in part: process registry with heartbeats, the supervisor failing claimed jobs of
  dead processes, recurring de-duplication on (task, run_at), capped counts. We add per-job leases, deadlines,
  cancellation and fencing, which it lacks.

## Consequences

About 3,000 lines we own and test (crash, fencing, leader failover, outage and SIGKILL tests). Postgres only; the
SQLite deployment has no jobs. Rails gets the same schema later.
```

- [ ] **Step 5: Record the spec deviations**

Append to the spec a section:

```markdown
## 22. Changes made while planning

- Recurring tasks run on a fixed interval (`every`, at least 1 s), not cron; ticks are multiples of the interval
  since the epoch in database time, still de-duplicated on (task, run_at).
- No `enqueue_many`: callers loop inside their transaction (the loan events do).
- No `trace` column.
- Environment names carry units: `JOBS_POLL_MS`, `JOBS_SHUTDOWN_TIMEOUT_S`, `JOBS_KEEP_SUCCEEDED_H`,
  `JOBS_KEEP_FAILED_H`, `JOBS_SMTP_TIMEOUT_S`. The lease is per kind in code (`JobOptions::lease`, default 60 s),
  not an environment variable; the heartbeat is 20 s.
- The jobs migration table is `_jobs_migrations`, set with sqlx's `Migrator::dangerous_set_table_name`, not
  `sqlx.toml`.
- Emails are enqueued only when `JOBS_SMTP_URL` is set, webhooks only for configured endpoints; a worker registers
  only the kinds it has settings for, so jobs wait (and `jobs status` names them) rather than fail.
- The real-SIGKILL test re-runs the crate's test binary as the child worker (`crates/jobs/tests/child_process.rs`);
  the binary itself is tested for start, readiness and SIGTERM in `apps/web/tests/jobs_worker.rs`.
- The outage test pauses a dedicated Postgres container (`docker pause`) and is `#[ignore]`d in CI.
```

- [ ] **Step 6: Soak: every suite 20 times**

```bash
set -o pipefail
export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh); eval "$(./scripts/test-mailpit.sh)"
for i in $(seq 1 20); do
  cargo test -q -p library-jobs 2>&1 | tail -3 || { echo "library-jobs failed on run $i"; break; }
  cargo test -q -p library-web --features postgres --test jobs_events --test jobs_email --test jobs_email_chaos --test jobs_webhook --test jobs_worker 2>&1 | tail -3 || { echo "web jobs failed on run $i"; break; }
done
```
Expected: 20 clean runs. A failure is a bug (usually a race in a test's waiting): fix it, then restart the count.

- [ ] **Step 7: Contract test with jobs on**

```bash
make loadtest-image
PREFIX=lt-ct- LT_DB=postgres NODE_CPUS=6,7 K6_CPUS=8,9 CENTRAL_CPUS=8,9 BOOKS=2000 MEMBERS=500 LOANS=1000 COOL_C=0 SEED_CACHE=0 \
  COMPOSE_PROFILES=jobs JOBS_ENABLED=true JOBS_SMTP_URL=smtp://mailpit:1025 scripts/loadtest.sh up
NGINX=$(docker inspect -f '{{(index .NetworkSettings.Networks "lt-ct-web_default").IPAddress}}' lt-ct-web-nginx-1)
CT_PSQL="docker exec -i lt-ct-web-postgres-1 psql -U library -d library -tA" scripts/contract-test.sh "http://$NGINX"
docker exec lt-ct-web-worker-1 library-web jobs status
PREFIX=lt-ct- LT_DB=postgres scripts/loadtest.sh down
```
Expected: the contract test reports 0 failed (402 checks at the time of writing); `jobs status` shows the email
queue with no failed jobs and the worker live. If `scripts/loadtest.sh` does not pass `COMPOSE_PROFILES` through to
`docker compose`, export it before the command; if it starts compose in a way that drops it, report that instead of
editing the runner.

- [ ] **Step 8: Request-path smoke (spec §21)**

With the jobs box from Step 7 still up (run this before its `down`), time 200 borrow + return pairs, one at a time:

```bash
B="http://$NGINX"; J=$(mktemp -d)
curl -fsS -c "$J/c" -H 'content-type: application/json' -d '{"username":"admin","password":"loadtest-local-only"}' "$B/api/auth/login" >/dev/null
book=$(curl -fsS -b "$J/c" -H 'content-type: application/json' -d '{"title":"smoke","author":"a","total_copies":1}' "$B/api/books" | jq .id)
member=$(curl -fsS -b "$J/c" -H 'content-type: application/json' -d "{\"name\":\"smoke\",\"email\":\"smoke$RANDOM@example.com\"}" "$B/api/members" | jq .id)
for i in $(seq 200); do
  out=$(curl -fsS -b "$J/c" -H 'content-type: application/json' -w '\n%{time_total}' -d "{\"book_id\":$book,\"member_id\":$member}" "$B/api/loans")
  echo "$out" | tail -1 >> "$J/borrow.txt"
  id=$(echo "$out" | head -1 | jq .id)
  curl -fsS -b "$J/c" -H 'content-type: application/json' -o /dev/null -w '%{time_total}\n' -d '{}' "$B/api/loans/$id/return" >> "$J/return.txt"
done
for f in borrow return; do sort -n "$J/$f.txt" | awk -v f=$f '{a[NR]=$1} END {printf "%s p50 %.2f ms p95 %.2f ms\n", f, a[int(NR*0.5)]*1000, a[int(NR*0.95)]*1000}'; done
```

Then bring the box up again without `COMPOSE_PROFILES=jobs JOBS_ENABLED=true JOBS_SMTP_URL=…`, repeat, and take it down.
Expected: p95 with jobs on is within about 1 ms of jobs off. Sequential curl is noisy; report both pairs of numbers
as they are. The measurement under load is sub-project 5.

- [ ] **Step 9: Final checks and commit**

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p library-web --all-targets --features postgres -- -D warnings
test -z "$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)"
cargo test -p library-jobs --test outage -- --ignored
git add -A deploy .github Makefile docs .env.example
git commit -m "jobs: compose worker profile, CI with Mailpit and web Postgres tests, JOBS.md, runbook, decision 0007, spec deviations"
```

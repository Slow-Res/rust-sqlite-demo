# jobwright, sub-project A (core API completeness): design

Date: 2026-10-18. Status: design agreed in conversation; this document awaits review.
Builds on: `2026-10-14-background-jobs-design.md` (v1, "the v1 spec"). Everything in the v1
spec still holds unless this document changes it.

## 1. Goal

Turn the v1 job queue (`crates/jobs`, package `library-jobs`) into **jobwright 0.1**: a
general-purpose, production-grade Postgres job processor for Rust ("Solid Queue for Rust";
product shape like Oban and River). Sub-project A closes the core API gaps found by auditing
seven libraries (Solid Queue, BullMQ, apalis, fang, underway, hammerwork, pgqueue) and moves the
crate into its own repository.

jobwright keeps v1's core unchanged: enqueue inside the caller's transaction (outbox), per-job
leases with fenced writes, one heartbeat per process independent of job tasks, cancellation of
running jobs from any process, database time everywhere, process registry and maintenance
leader, bounded shutdown, no exit on a database outage.

### 1.1 Roadmap (each sub-project gets its own spec, plan and build)

| # | Sub-project | Status |
|---|---|---|
| **A** | **Core API completeness + own repository** | **this spec** |
| B | Results, events, hooks (stored return values, wait-for-completion, event stream, lifecycle hooks, tracing spans and trace propagation) | later |
| C | Flow control (runtime limits stored in the DB, global rate limits, pause on 429, circuit breaker, fair sharing) | later |
| D | Workflows (chains, parent/child, batches) | later |
| E | Process supervisor (several isolated worker processes per host) | later, lowest priority |
| F | Admin JSON API and web dashboard, with auth | later |

### 1.2 Decisions taken in conversation (2026-10-18)

1. The crate is named **jobwright** and lives in its own repository.
2. Every per-kind setting has three layers: code default → deployment override → per-call
   override (§5).
3. Concurrency per key allows **N** running jobs (configurable like any other setting), not only 1.
   No throttle window, keep-last-while-running or discard-on-conflict modes.
4. Recurring tasks are defined **both** in code (static) and in the database (dynamic), with cron
   schedules, time zones and a missed-run policy.
5. jobwright 0.1 ships **one clean initial migration** (v1's schema plus A's changes). No deployed
   database carries v1 jobs: production runs SQLite and every Postgres database so far is a test or
   load-test database.

### 1.3 Non-goals for A

Everything in B to F. SQLite, MySQL or any other backend. Throttle windows, keep-last and
discard-on-conflict de-duplication. Backtrace capture. Exactly-once side effects (delivery stays at
least once).

## 2. Repository and packaging

### 2.1 Extraction

- New repository `~/rust/jobwright`, created by extracting `crates/jobs` from
  `~/rust/rust-sqlite-demo` with its git history (`git filter-repo --subdirectory-filter
  crates/jobs` on a fresh clone). Local only; creating a GitHub repository is a separate decision.
- This spec, the v1 spec and the generic parts of `docs/JOBS.md` are copied into the new repo
  (`docs/`); app-specific docs stay in rust-sqlite-demo.

### 2.2 Layout

```
jobwright/                 Cargo workspace
  jobwright/               library crate `jobwright` (MIT, 0.1.0)
    src/  migrations/  tests/  examples/
  jobwright-cli/           binary `jobwright` (operator CLI, §10)
  docs/                    specs, guide (JOBS.md successor), CHANGELOG.md at the root
  .github/workflows/ci.yml
```

- Features: `http` (axum metrics/health server, as v1), `testing` (§11.1).
- CI: fmt, clippy `-D warnings` (all features and default), tests against Postgres 16, 17 and 18,
  `cargo doc` with `-D warnings`, the example builds.
- No app-specific code, names or docs in the crate. Error and log text says "jobwright".
- Migrations use their own history table `_jobwright_migrations`.

### 2.3 rust-sqlite-demo after extraction

- Depends on `jobwright = { path = "../jobwright/jobwright" }` (git dependency once a remote exists).
- `crates/jobs` is deleted. `apps/web/src/jobs/` keeps the app jobs (`loan_email.v1`,
  `webhook.v1`), config and the `worker` command, adapted to the 0.1 API.
- `library-web jobs …` is removed; docs and RUNBOOK point to the `jobwright` CLI.
- Test and load-test databases are recreated with the new schema. The idle `lt-pg-web-*` load-test
  stack is not touched by this work.

## 3. Data model (the 0.1 initial migration)

The full v1 schema (v1 spec §4) with these changes. Column comments in the migration explain each.

### 3.1 `jobs`: new and changed columns

| Column | Type | Meaning |
|---|---|---|
| `scheduled_at` | `TIMESTAMPTZ NOT NULL` | The first `run_at` the job was enqueued with; never changes. For recurring jobs, the tick time. Read by `ctx.scheduled_at()`. |
| `backoff_base_ms`, `backoff_cap_ms` | `INT NOT NULL` | Resolved backoff (§5); v1 took it from the registry at failure time. |
| `concurrency_limit` | `SMALLINT NOT NULL DEFAULT 1` | Max running jobs sharing this `concurrency_key` (1..=1000). |
| `concurrency_slot` | `SMALLINT` | Set only while running with a key: which of the `concurrency_limit` slots it holds. |
| `keep_succeeded_s`, `keep_failed_s` | `INT NOT NULL` | Resolved retention (§5). `cancelled` uses `keep_failed_s`. |
| `delete_after` | `TIMESTAMPTZ` | Set when the job reaches a terminal state: `finished_at` + the matching keep time. |
| `errors` entries | JSONB | `{attempt, at, cause, type, message}` (adds `type`, §8.1). |

Constraints and indexes:

- `jobs_slot_iff_running_keyed`: `(concurrency_slot IS NOT NULL) = (state = 'running' AND
  concurrency_key IS NOT NULL)`.
- `jobs_slot_in_range`: `concurrency_slot IS NULL OR (concurrency_slot >= 0 AND concurrency_slot <
  concurrency_limit)`.
- `jobs_delete_after_iff_terminal`: `(delete_after IS NOT NULL) = (state IN ('succeeded','failed','cancelled'))`.
- Unique index `jobs_running_slot ON jobs (concurrency_key, concurrency_slot) WHERE concurrency_slot
  IS NOT NULL` replaces v1's `jobs_one_running_per_key`.
- `jobs_expire ON jobs (delete_after) WHERE delete_after IS NOT NULL` replaces `jobs_finished`.
- `jobs_state_id ON jobs (state, id)` and `jobs_kind_id ON jobs (kind, id)` for paged listing (§10.1).
- v1's `jobs_lease_iff_running`, `jobs_finished_iff_terminal`, `jobs_claim`, `jobs_running_lease`,
  `jobs_running_holder` and `jobs_unique` are kept as they are.

Every UPDATE that moves a row out of `running` clears `locked_by`, `locked_until` and
`concurrency_slot` in the same statement; every UPDATE into a terminal state sets `finished_at` and
`delete_after` in the same statement. The constraints make a missed column a hard error.

### 3.2 `job_logs` (new)

```sql
CREATE TABLE job_logs (
    id      BIGINT GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY,
    job_id  BIGINT NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    attempt INT NOT NULL,
    at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    message TEXT NOT NULL CHECK (length(message) <= 1000)
);
CREATE INDEX job_logs_job ON job_logs (job_id, id);
```

### 3.3 `job_recurring_tasks` (new)

```sql
CREATE TABLE job_recurring_tasks (
    name        TEXT PRIMARY KEY,
    source      TEXT NOT NULL CHECK (source IN ('static', 'dynamic')),
    in_code     BOOLEAN NOT NULL DEFAULT true,   -- static only: false when the leader's code no longer declares it
    kind        TEXT NOT NULL,
    args        JSONB NOT NULL,
    options     JSONB NOT NULL DEFAULT '{}',     -- per-task overrides (§5), same shape as Overrides
    schedule    TEXT NOT NULL,                   -- cron (5 or 6 fields) or "every <duration>"
    time_zone   TEXT NOT NULL DEFAULT 'UTC',     -- IANA name
    misfire     TEXT NOT NULL DEFAULT 'run_once' CHECK (misfire IN ('run_once', 'skip')),
    enabled     BOOLEAN NOT NULL DEFAULT true,   -- false = paused
    last_run_at TIMESTAMPTZ,                     -- last tick enqueued
    next_run_at TIMESTAMPTZ,                     -- next tick due (database time)
    status      TEXT,                            -- last problem, e.g. "kind not registered on leader"
    updated_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_by  TEXT NOT NULL
);
CREATE INDEX job_recurring_due ON job_recurring_tasks (next_run_at) WHERE enabled;
```

`job_recurring_runs` keeps its role (one row per tick, exactly-once enqueue) with `run_at` renamed
`fire_at` and a new `manual BOOLEAN NOT NULL DEFAULT false`. Kept 7 days, as in v1.

### 3.4 Unchanged tables

`job_processes`, `job_queue_pauses`, and the `jobs_push_error` function (now storing `type`).

## 4. Job definition API

```rust
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    const KIND: &'static str;
    const QUEUE: &'static str;               // the default queue; overridable (§5)
    fn options() -> JobOptions { JobOptions::default() }
    fn unique_key(&self) -> Option<String> { None }
    fn concurrency_key(&self) -> Option<String> { None }
    fn perform(self, ctx: JobContext) -> impl Future<Output = Result<(), JobError>> + Send;
}

pub struct JobOptions {
    pub max_attempts: i32,           // 1..=1000 (v1)
    pub max_crashes: i32,
    pub deadline: Duration,
    pub lease: Duration,
    pub priority: i16,
    pub backoff: Backoff,
    pub concurrency_limit: i16,      // new, default 1, 1..=1000
    pub keep_succeeded: Duration,    // new, default 24 h
    pub keep_failed: Duration,       // new, default 14 days (applies to failed and cancelled)
}
```

`MaintenanceConfig::keep_succeeded` and `keep_failed` are removed; retention is per row (§9).

## 5. Settings in three layers

```rust
/// Every field optional; `None` keeps the value from the layer below.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct Overrides {
    pub queue: Option<String>,
    pub priority: Option<i16>,
    pub max_attempts: Option<i32>,
    pub max_crashes: Option<i32>,
    pub deadline: Option<Duration>,      // serde: humantime strings ("15m")
    pub lease: Option<Duration>,
    pub backoff_base: Option<Duration>,
    pub backoff_cap: Option<Duration>,
    pub concurrency_limit: Option<i16>,
    pub keep_succeeded: Option<Duration>,
    pub keep_failed: Option<Duration>,
}

/// Deployment layer: per-kind overrides, loaded by the app from any config source.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct KindOverrides(pub BTreeMap<String, Overrides>);
impl KindOverrides {
    /// Compact form for an environment variable:
    /// "webhook.v1:max_attempts=5,concurrency_limit=3;loan_email.v1:queue=mail"
    pub fn parse(raw: &str) -> Result<Self, ConfigError>;
}
```

Resolution, lowest to highest: `Job::QUEUE` + `Job::options()` → `KindOverrides` entry for the kind
→ `EnqueueOptions::overrides` (or the recurring task's `options`). The result is validated
(`JobOptions::validate`, plus queue name rules) and **stored on the row at enqueue**, as v1 stores
its options. A job therefore behaves the same whichever worker runs it.

- The crate never reads environment variables. `KindOverrides` is given to the `Enqueuer` and to
  the `Worker` (which uses it for `ctx.enqueue` and recurring enqueues). Every process that
  enqueues must load the same overrides; the guide says so.
- Unknown kinds in `KindOverrides` are allowed (a web process may enqueue kinds it never runs) but
  the worker logs them once at start.
- A `lease` override shorter than the worker's shortest registered lease is lifted by the claim's
  existing `GREATEST(lease_s, shortest)` floor (v1 §22, M2).
- **Queue overrides change claiming:** v1 claimed from queue `q` only the kinds whose `Job::QUEUE`
  is `q`. jobwright claims from each served queue any kind the process registers (a kind may now
  sit in any queue). A job in a queue no process serves waits there, visible in `status`.

## 6. Enqueue

```rust
pub struct EnqueueOptions {
    pub delay: Option<Duration>,
    pub run_at: Option<DateTime<Utc>>,   // new; setting both delay and run_at is an error
    pub replace: bool,                   // v1
    pub overrides: Overrides,            // new: the per-call layer (priority moves in here)
}

pub struct Enqueuer { /* notify: bool, overrides: KindOverrides */ }
impl Enqueuer {
    pub fn new() -> Self;                                   // notify on, no overrides
    pub fn notify(self, on: bool) -> Self;
    pub fn overrides(self, o: KindOverrides) -> Self;
    pub async fn enqueue<J: Job>(&self, conn: &mut PgConnection, job: &J) -> Result<Enqueued, EnqueueError>;
    pub async fn enqueue_with<J: Job>(&self, conn: &mut PgConnection, job: &J, opts: &EnqueueOptions) -> Result<Enqueued, EnqueueError>;
    /// One INSERT for all jobs; returns one outcome per job, in order.
    pub async fn enqueue_many<J: Job>(&self, conn: &mut PgConnection, jobs: &[(J, EnqueueOptions)]) -> Result<Vec<Enqueued>, EnqueueError>;
}
```

- `run_at` in the past means "now". `scheduled_at` = the resolved first `run_at`.
- **`enqueue_many`**: validates every job first (args size, options); if any is invalid, nothing is
  written and the error names its index. Then one `INSERT … SELECT … FROM unnest($1::text[], …)`
  with the same de-duplication rules as `enqueue` per row (`ON CONFLICT` on `jobs_unique`; `replace`
  rows use the v1 replace path, run as a second statement for those rows only). One `pg_notify`
  per distinct queue. Limit 10,000 jobs per call (`EnqueueError::TooMany`). Mixed kinds go through
  `enqueue_many_raw(&[RawJob])` (the typed version is the common case).
- **`ctx.enqueue(&job)` / `ctx.enqueue_with(&job, opts)`** inside a handler: the job is validated
  at once (errors return to the handler) and buffered. When the handler returns `Ok`, the buffered
  jobs are inserted **in the same transaction as the fenced success write**. If the success write
  matches no row (lease lost, fenced), the transaction rolls back and nothing is enqueued. On any
  other outcome the buffer is dropped. At most 1,000 buffered jobs per attempt. A database error in
  this transaction is handled like any failed completion write (v1 §11.5: retried in memory until it
  lands or the fence rejects it).

## 7. N running jobs per key

- A job with `concurrency_key = K` may start only when fewer than its `concurrency_limit` jobs with
  key `K` are running. The limit is a resolved setting (§5), default 1 (v1 behaviour).
- **Claim** (replaces v1 `claim_keyed`'s single-slot check): for a keyed candidate, take the existing
  `pg_try_advisory_xact_lock(hashtextextended('jobs.key.' || K, 0))`; on a miss skip the key. On a
  hit, pick the lowest free slot:
  `SELECT s FROM generate_series(0, candidate.concurrency_limit - 1) s WHERE NOT EXISTS (SELECT 1
  FROM jobs WHERE concurrency_key = K AND concurrency_slot = s) LIMIT 1`. No free slot → skip the
  key. Otherwise claim the candidate with that slot. The unique index `jobs_running_slot` is the
  database-level backstop; a violation there is treated like v1's: skip, never error.
- Rows of one key with different limits are each checked against their own limit (a row with limit 1
  will not start while any other row of the key runs). The guide documents this.
- Slots are freed by every write that leaves `running` (§3.1): success, failure, retry, release,
  cancel, reaper, prune.
- The per-queue `max_running` limit (v1 §7.2) is unchanged and applies on top.

## 8. Failure detail and per-job log

### 8.1 Error type

```rust
impl JobError {
    pub fn retry<E: fmt::Display + 'static>(err: E) -> Self;      // v1 signature, now records type
    pub fn permanent<E: fmt::Display + 'static>(err: E) -> Self;
    pub fn with_type(self, code: impl Into<String>) -> Self;        // explicit type, e.g. "smtp.timeout"
}
```

- `type` = the explicit code if set, else `std::any::type_name::<E>()` (e.g.
  `reqwest::error::Error`), truncated to 200 characters.
- `message` = `format!("{err:#}")` (alternate Display; anyhow and many error types print their
  source chain this way), then v1's NUL stripping and 2,000-character cap.
- Crate-produced failures use fixed types: `panic`, `deadline_exceeded`, `lease_expired`,
  `process_lost`, `bad_args`, `unknown_kind`, `cancelled`.
- Logs (`jobs.failed`) gain a `type` field. No backtraces.

### 8.2 Per-job log

- `ctx.log(message)` buffers a line (truncated to 1,000 characters, NUL stripped). A per-process
  flusher inserts buffered lines every second in one statement, like progress (v1 §10).
- At most 100 lines per job: the flusher inserts only while the job has fewer; on reaching the cap
  it writes one final line `log truncated (100 lines)`.
- Lines are deleted with their job (FK cascade) by retention.
- Operator actions (§10.2) add a line to each affected job, e.g. `discarded by <who>: <reason>`.
- The guide warns: never log secrets or personal data; the log is visible to operators.

## 9. Retention

- `delete_after` is set when a job turns terminal (§3.1). The leader deletes rows with
  `delete_after < now()` in batches of 1,000 (v1 batching), using `jobs_expire`.
- `ops::delete` (§10.2) is the on-demand clean.
- `job_recurring_runs` retention stays 7 days.

## 10. Operations

All operator functions live in `jobwright::ops`, take `&PgPool`, and are the API sub-project F
will expose over HTTP. The `jobwright` CLI calls them one-to-one.

### 10.1 Listing

```rust
pub struct JobFilter {
    pub states: Vec<State>, pub queue: Option<String>, pub kind: Option<String>,
    pub unique_key: Option<String>, pub error_type: Option<String>,
    pub created: Option<TimeRange>, pub scheduled: Option<TimeRange>, pub finished: Option<TimeRange>,
    pub ids: Vec<i64>,
}
pub async fn list(pool, f: &JobFilter, page: Page) -> Result<(Vec<JobSummary>, Option<Cursor>), _>;
pub async fn count(pool, f: &JobFilter) -> Result<Count, _>;   // Count::Exact(n) or Count::AtLeast(10_000)
pub async fn show(pool, id) -> Result<Option<JobDetail>, _>;    // + last 100 log lines
```

- Pages are keyset on `id` descending (`id < cursor`), default 50, max 1,000.
- `error_type` matches the latest error entry's `type`.

### 10.2 Bulk actions

Each takes a `JobFilter`, an operator name and an optional reason, and returns the number of jobs
affected. An empty filter is refused unless the call passes `All` explicitly.

| Function | Acts on | Effect |
|---|---|---|
| `discard` | waiting (`available`) | → `cancelled` |
| `cancel` | waiting and running | waiting → `cancelled`; running → `cancel_requested` (v1 cancel) |
| `retry` | `failed`, `cancelled` | → `available`, attempt and crashes reset, errors kept (v1 retry rules, including the unique-key race) |
| `run_now` | `available` with `run_at > now()` | `run_at = now()` |
| `delete` | terminal states only | rows deleted (logs cascade) |

- Each action runs in batches of 1,000 rows, one transaction per batch, so a large action never
  holds locks long; the result is the sum. Each batch also appends the operator line (§8.2).
- "Drain a queue" is `discard` with a queue filter; "cancel by business key" is `cancel` with
  `unique_key`.
- v1's per-id `retry`, `discard`, `cancel` remain as convenience wrappers over the filter forms.

### 10.3 Recurring tasks

`recurring_list` (all tasks with last/next run, `in_code`, `status`), `recurring_show` (task plus
last 50 runs joined to their jobs), `recurring_upsert` (dynamic only), `recurring_delete` (dynamic,
or static with `in_code = false`), `recurring_pause`, `recurring_resume`, `recurring_run_now`.

### 10.4 Queues and workers

v1 `status`, `pause`, `resume`, `processes`; `processes` now includes each process's running jobs
(id, kind, attempt, started).

### 10.5 The `jobwright` CLI

- `jobwright <command>`; database from `--database-url` or `DATABASE_URL`; `--json` for machine
  output, tables otherwise.
- Commands: `migrate`, `status`, `list`, `count`, `show`, `discard`, `cancel`, `retry`, `run-now`,
  `delete`, `pause`, `resume`, `processes`, `recurring {list,show,upsert,delete,pause,resume,run-now}`.
- Destructive commands print the matching count and require `--yes` or an interactive confirmation.
  `--who` defaults to `$USER@$HOSTNAME`.

## 11. Recurring tasks

### 11.1 Schedules

- `schedule` is a cron expression (5 fields, or 6 with seconds first) parsed with `croner`, or
  `every <duration>` (humantime, ≥ 1 s; ticks aligned to multiples of the duration since the Unix
  epoch in UTC, as v1).
- `time_zone` is an IANA name resolved with `chrono-tz`. DST follows `croner`: a local time that
  does not exist that day is skipped; one that occurs twice fires once.
- Validation at declaration and at upsert: the schedule parses, the zone exists, and the schedule
  has a next fire time within 5 years ("never fires" is rejected, e.g. `0 0 30 2 *`).

### 11.2 Static tasks

```rust
registry.recurring("nightly-report", Schedule::cron("0 2 * * *")?.tz("Europe/Berlin")?, ReportJob { .. })
        .misfire(Misfire::RunOnce).options(Overrides { .. });
```

- Args are a fixed value (v1's closure is removed); handlers needing the tick time use
  `ctx.scheduled_at()`.
- When a process becomes leader it upserts every static task it declares (`source = 'static'`,
  `in_code = true`), updating kind, args, options, schedule, zone and misfire but **not** `enabled`
  or the run times. Static rows it does not declare get `in_code = false` and are not run.
- A static declaration whose name belongs to a dynamic task is not applied; the leader logs an
  error and sets that row's `status`.
- Static tasks can be paused, resumed and run now; editing them is refused (they are owned by code).

### 11.3 Dynamic tasks

`ops::recurring_upsert(pool, task, who)` validates (§11.1, args ≤ 64 KiB, kind name rules) and
writes with `source = 'dynamic'`; `next_run_at` is computed at once. An upsert for a kind no live
process registers (per `job_processes.config`) succeeds with a warning in the result.

### 11.4 The tick (leader only)

Every `recurring_every` (default 1 s):

1. In one transaction: `SELECT … FROM job_recurring_tasks WHERE enabled AND next_run_at <= now()
   FOR UPDATE SKIP LOCKED LIMIT 100`, and read `now()`.
2. For each task: the due fire `f` is the latest fire time ≤ now. If `f` ≥ now − `recurring_grace`
   (default 5 min, `MaintenanceConfig::recurring_grace`) it is on time. Otherwise ticks were missed
   (no leader, outage): with `run_once` enqueue `f` once; with `skip` enqueue nothing. At most one
   tick per task per pass: ticks between `last_run_at` and `f` are never replayed (as in v1).
3. Enqueue = `INSERT INTO job_recurring_runs (task, fire_at) … ON CONFLICT DO NOTHING`, and only if
   a row was inserted, enqueue the job (resolved options: kind defaults → `KindOverrides` → task
   `options`; `run_at = scheduled_at = f`) and store its id in the run row.
4. Set `last_run_at = f` (if enqueued) and `next_run_at` = the first fire time > now, computed in the
   task's zone from the database's `now()`. Clear `status`.
5. A task whose kind the leader does not register is not enqueued: `status = 'kind not registered on
   leader'`, a warning once per task per leadership, and `jobs_recurring_skipped_total{task}`. The
   guide recommends registering every kind on every worker (registering a kind does not make a
   process serve its queue).

`recurring_run_now` inserts a run with `fire_at = now()`, `manual = true`, and enqueues at once,
without changing `next_run_at`.

## 12. Migrations

- The 0.1 initial migration contains the whole schema (§3).
- `jobwright::migrate(pool)` runs the embedded migrations. Each migration statement runs with
  `SET lock_timeout = '5s'`; a lock timeout rolls back that migration and retries with backoff
  (1 s → 30 s, 10 attempts), so a migration never queues behind long transactions while blocking
  application queries. Concurrent migrators are serialised by sqlx's advisory lock.

## 13. Testing helpers (feature `testing`)

- `TestDb::new().await` creates a uniquely named database from `TEST_POSTGRES_URL`, migrates it,
  and drops it on `close()` (and best-effort on drop).
- `drain(pool, &registry) -> Vec<Outcome>` claims and runs every claimable job in-process, one at a
  time, until none is claimable (including follow-ups from `ctx.enqueue`), and returns each outcome.
- `advance(pool, by)` simulates elapsed time by moving `run_at`, `locked_until` and recurring
  `next_run_at` back by `by`. The database clock stays real.
- `assert_enqueued::<J>(pool, |job: &J| …)` finds a waiting job of kind `J` matching the predicate.

## 14. Metrics and logs

v1's metrics and log events stay (v1 §17, §22). New:

- `jobs_recurring_enqueued_total{task}`, `jobs_recurring_skipped_total{task}`,
  `jobs_recurring_missed_total{task,policy}`.
- `jobs_enqueued_total{kind}` counts `enqueue_many` rows individually.
- Log events: `jobs.recurring.enqueued`, `jobs.recurring.missed`, `jobs.recurring.skipped`,
  `jobs.ops.action` (operator, action, filter summary, count). `jobs.failed` gains `type`.
- Metric names keep the `jobs_` prefix (dashboards and alerts from v1 keep working).

## 15. Testing (how A is verified)

- All v1 tests move with the crate and pass, including the outage, SIGKILL and fence tests.
- New tests per section, at least:
  - §5: each layer wins over the one below; invalid resolved options are rejected at enqueue; a
    queue override is claimed by a worker serving that queue.
  - §6: `enqueue_many` of 10,000 rows is one INSERT (statement count) and dedups per row; an invalid
    row writes nothing; `ctx.enqueue` jobs appear only after success, never after failure or a
    fenced success write.
  - §7: 3 worker processes, 20 jobs with limit 3 on one key: a sampler never sees more than 3
    running; a crashed job's slot is freed by the reaper; limit 1 matches v1 behaviour.
  - §8: error `type` recorded for typed errors and `with_type`; log cap and truncation line; log rows
    cascade on delete.
  - §10: every bulk action with filters, batching over 2,500 rows, the empty-filter refusal, cursor
    pages without gaps or repeats while rows are inserted.
  - §11: cron in a DST zone across both transitions; `run_once` and `skip` after a simulated outage;
    two leaders racing one tick enqueue it once; static sync during a rolling deploy (old and new
    leaders alternate) never runs a removed task and never loses `enabled = false`; dynamic CRUD
    and validation errors.
  - §12: a migration waiting on a lock times out and retries instead of blocking a concurrent
    query.
- Throughput benchmark (example `throughput`, same box and method as v1): `enqueue_many` vs a loop
  of `enqueue`; no-op claim/finish throughput within 10 % of v1 (≈ 19,000 jobs/s, 4 cores); keyed
  jobs with limit 3.
- rust-sqlite-demo: its full suite, the contract test with jobs enabled, and the Mailpit tests pass
  on jobwright.

## 16. Acceptance

A is done when: the jobwright repository exists locally with history, CI config and docs; every
section above is implemented and tested as in §15 on Postgres 16, 17 and 18; the benchmark shows no
regression beyond 10 %; rust-sqlite-demo runs on jobwright with its suites green; the guide,
README and CHANGELOG describe the 0.1 API; and a final whole-branch review finds no Critical or
Important issue left open.

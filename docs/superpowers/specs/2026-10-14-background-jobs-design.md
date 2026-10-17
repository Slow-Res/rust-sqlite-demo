# Background jobs (transactional outbox + job queue): design

Date: 2026-10-14. Status: design agreed in conversation; this document awaits review.

## 1. Goal

A production-grade way to run anything slow or fallible outside the request: email, outgoing
webhooks, and later report generation, data imports and incoming-webhook processing. A job
is written **in the same Postgres transaction** as the data that caused it (the outbox
guarantee), and is then run by workers with retries, crash recovery, cancellation, progress,
limits, monitoring and operator tooling.

**Delivery guarantee: at least once.** A job runs to completion at least once unless it is
cancelled or gives up after its attempts. It can run more than once only when a worker loses
its lease mid-job (crash, kill, freeze, network split). Every handler is therefore written
to be idempotent; §15 shows how the first two are.

### Sub-projects

This document specifies sub-project 1 in full and fixes the core so that 2 and 3 need no
changes to it.

| # | Sub-project | Status |
|---|---|---|
| **1** | **Job system core + loan email + outgoing webhooks** | **this spec** |
| 2 | Reports and imports (file storage, upload/download endpoints, streaming) | later; uses §8 deadlines, §10 progress/cancel, §12 limits |
| 3 | Incoming webhooks (verify, store, 2xx, process later) | later |
| 4 | Rails counterpart (Solid Queue in the same database, same jobs) | later |
| 5 | Load test with mock SMTP/HTTP services with latency, Rust vs Rails | later |

### Non-goals (this sub-project)

- SQLite. Jobs need Postgres (§3.3). The SQLite product build does not get jobs yet.
- Parent/child flows, batches, a web dashboard (the ops CLI in §17 comes first; a page can
  later reuse its queries).
- Exactly-once side effects. Not achievable with SMTP or third-party HTTP; we get
  at-least-once plus idempotency keys.
- A global (cross-process) rate limiter. Rate limits are per worker process in v1 (§12.3).

## 2. Why our own crate

We compared four libraries by reading their source (2026-10-14): BullMQ's Rust port
(`bullmq-official` 1.3.6), `fang` 0.11, `apalis` + `apalis-postgres` 1.0, and Rails'
Solid Queue 1.7 (as a design reference).

| Need | BullMQ (Rust) | fang | apalis-postgres | Solid Queue |
|---|---|---|---|---|
| Enqueue in the app's transaction | no (Redis) | no (own pool, sqlx 0.8) | yes (`push_tasks(&mut *tx)`) | only with the same DB (not the default) |
| Recovery of one stuck/long job | per-job lock, but a lost lock does not stop the job | none (stuck forever) | per-worker heartbeat; one job may run forever; heartbeat shares the job's task | per-process only; "a stuck job can remain claimed" |
| Stale-worker fencing | lock token | none | worker name only | claim row lock |
| Backoff stored in the DB | yes | yes | no (immediate retries) | via Active Job |
| Cancel a running job from anywhere | no | no | no | no |
| Progress | yes | no | no | no |
| Poison job (kills its worker) | `max_stalled_count` | loops | loops | fails it (no retry) |

None meets the outbox + long-job requirements without patching its core. The design below
takes the claim SQL from apalis/Solid Queue, the lock-token/stalled-job model from BullMQ, and
the process registry, death causes, recurring de-duplication, pauses and monitoring from
Solid Queue; it uses the database clock throughout.

## 3. Architecture

### 3.1 Code layout

| Path | What | Knows about the library app? |
|---|---|---|
| `crates/jobs/` (crate `library-jobs`) | Enqueue API, worker runtime, process registry, maintenance, metrics, ops queries, its SQL migrations. | **No.** Generic: job kinds are registered by the app. |
| `apps/web/src/jobs/` | The app's job types and handlers (`loan_email`, `webhook`), their config, the `worker` and `jobs` subcommands, wiring. | Yes |
| `apps/web/src/loans/postgres.rs` | Enqueues loan events inside the existing borrow/return transactions. | Yes |

`crates/jobs` is a library with no `main`. It depends on `sqlx` 0.9 (postgres), `tokio`,
`tokio-util` (cancellation tokens), `serde`/`serde_json`, `tracing`, `prometheus-client`
(already used by the sync worker), `rand` (jitter). The web crate enables it only under its
existing `postgres` feature.

### 3.2 Process model: one binary, two roles

- `library-web serve` (unchanged) serves HTTP and only **enqueues**. It runs no job code.
- `library-web worker` (new) runs workers, heartbeats and maintenance, and serves
  `/metrics`, `/healthz`, `/readyz` on its own port (default 9300). Same image, same binary.
- **Embedded mode** (`JOBS_EMBEDDED=true` on `serve`) runs the worker inside the web
  process, for development and very small deployments. Off by default.

The separate process is the production default: a 20-minute report or a 500 MB import must
not slow requests, crash the API, or die when the API restarts, and workers scale on their
own (more processes, or another VM).

### 3.3 Database

Postgres only, the same database as the app (that is what makes it an outbox). The schema
lives in its own migration set `crates/jobs/migrations/` and its own sqlx migration table
(`_sqlx_jobs_migrations` via the migrator's table option); `library-web serve` and `worker`
run both migrators at start-up (app first). This keeps the crate's schema versioned with the
crate. The separate table name uses sqlx 0.9's per-crate `sqlx.toml` (`migrate.table-name`),
as apalis-postgres does; the plan verifies it first.

All times are database time (`now()`, `clock_timestamp()`); no process clock is ever
compared with a stored time, because workers may run on several VMs.

## 4. Data model

### 4.1 `jobs`

| Column | Type | Meaning |
|---|---|---|
| `id` | bigint identity PK | |
| `kind` | text | Job type with a version suffix, e.g. `loan_email.v1`. A payload change gets a new version, so old and new workers can coexist during a deploy. |
| `queue` | text | Named queue (§11). |
| `args` | jsonb | Small payload (target ≤ 8 KB, hard limit 64 KB checked at enqueue). Big data is referenced by id. |
| `state` | text | `available`, `running`, `succeeded`, `failed`, `cancelled` (CHECK constraint). |
| `priority` | smallint | Lower runs first (0 default). |
| `run_at` | timestamptz | Not before this time (delays, backoff). |
| `attempt` | int | Times claimed so far. |
| `max_attempts` | int | Default from the kind (email 10, webhook 15). |
| `crashes` | int | Attempts that ended by lease expiry or process loss (§9.3). |
| `max_crashes` | int | Default 2. |
| `deadline_s` | int | Longest a single attempt may run (§8.4). |
| `lease_s` | int | Lease length for this job (default 60). |
| `locked_by` | bigint NULL | `job_processes.id` of the holder. |
| `locked_until` | timestamptz NULL | Lease expiry. |
| `attempt_started_at` | timestamptz NULL | Start of the current attempt (for the deadline). |
| `unique_key` | text NULL | De-duplication key (§6.3). |
| `concurrency_key` | text NULL | At most one running job per key (§12.2). |
| `cancel_requested` | bool | Set by an operator or app; read on heartbeat (§10). |
| `progress` | jsonb NULL | Handler-reported progress (§10). |
| `errors` | jsonb | Last 10 errors: `{attempt, at, cause, message}`. |
| `trace` | text NULL | W3C `traceparent` of the enqueuing request. |
| `created_at`, `finished_at` | timestamptz | |

Indexes (all partial, so they stay small however many finished rows exist):

```sql
-- claiming: only waiting jobs
CREATE INDEX jobs_claim ON jobs (queue, priority, run_at, id) WHERE state = 'available';
-- reaper: only running jobs
CREATE INDEX jobs_running_lease ON jobs (locked_until) WHERE state = 'running';
CREATE INDEX jobs_running_holder ON jobs (locked_by) WHERE state = 'running';
-- de-duplication among jobs that have not finished
CREATE UNIQUE INDEX jobs_unique ON jobs (kind, unique_key)
  WHERE unique_key IS NOT NULL AND state IN ('available', 'running');
-- at most one running job per concurrency key
CREATE UNIQUE INDEX jobs_one_running_per_key ON jobs (concurrency_key)
  WHERE concurrency_key IS NOT NULL AND state = 'running';
-- retention and ops listings
CREATE INDEX jobs_finished ON jobs (finished_at) WHERE state IN ('succeeded', 'failed', 'cancelled');
```

Table settings: `fillfactor = 80` (room for same-page updates), per-table autovacuum
`vacuum_scale_factor = 0`, `vacuum_threshold = 1000`, `insert` variants likewise (the
settings that cut Postgres CPU in load-test round 18). If the load test (sub-project 5)
shows bloat or slow claims, the fallback is Solid Queue's split into a narrow
`ready` table; the API does not change.

### 4.2 `job_processes` (the registry)

`id` bigint identity PK, `name` text unique (`worker-<host>-<pid>-<random>`), `kind`
(`worker`, `embedded`), `hostname`, `pid`, `version` (crate version + git sha),
`config` jsonb (queues, concurrency, lease defaults), `started_at`, `last_heartbeat_at`,
`stopping` bool.

### 4.3 `job_queue_pauses`

`queue` text PK, `paused_at`, `paused_by` text, `reason` text. A paused queue is skipped by
claims (running jobs finish).

### 4.4 `job_recurring_runs`

`task` text, `run_at` timestamptz, `job_id` bigint NULL, `created_at`; PK `(task, run_at)`.
Kept 7 days, **independently of job retention** (Solid Queue's dedup rows vanish with the
jobs; ours do not).

## 5. Job definition API (app side)

```rust
#[derive(Serialize, Deserialize)]
pub struct LoanEmail { pub loan_id: i64, pub event: LoanEvent }

impl Job for LoanEmail {
    const KIND: &'static str = "loan_email.v1";
    const QUEUE: &'static str = "email";
    fn options() -> JobOptions { JobOptions { max_attempts: 10, deadline: secs(60), ..Default::default() } }
    async fn perform(self, ctx: JobContext) -> Result<(), JobError>;
    fn unique_key(&self) -> Option<String> { None }        // optional
    fn concurrency_key(&self) -> Option<String> { None }   // optional
}
```

`JobContext` gives the pool, the job id and attempt, `ctx.cancelled()` (a
`CancellationToken`), `ctx.set_progress(json)`, and `ctx.idempotency_key()`
(`"<kind>:<job id>"`, stable across attempts). A `Registry` maps `KIND` to a decoder +
handler; an unknown kind is never claimed by that process (§7.1), so a rolling deploy cannot
hand a new job to an old worker.

`JobError` kinds: `Retry(msg)` (default for `anyhow` errors), `RetryAfter(duration, msg)`
(e.g. HTTP 429 `Retry-After`), `Permanent(msg)` (no retry: bad input, 4xx), `Cancelled`.

## 6. Enqueue (the outbox)

### 6.1 API

```rust
let mut tx = pool.begin().await?;
let loan = insert_loan(&mut tx, ..).await?;
jobs::enqueue(&mut *tx, LoanEmail { loan_id: loan.id, event: Borrowed }).await?;
jobs::enqueue_with(&mut *tx, job, EnqueueOptions { run_at, priority, .. }).await?;
jobs::enqueue_many(&mut *tx, vec![..]).await?; // one INSERT … SELECT FROM unnest(…)
tx.commit().await?;
```

`enqueue` takes any `&mut PgConnection` (a transaction or a plain connection). It returns
`Enqueued::New(id)` or `Enqueued::Duplicate(existing_id)`.

### 6.2 Guarantees

- Rollback ⇒ the job never existed. Commit ⇒ it is durable and visible to workers.
- `enqueue` runs `SELECT pg_notify('jobs', queue)` in the same transaction; Postgres
  delivers it only at commit, so a worker never wakes before the job is visible.
- **Notify cost:** NOTIFY takes a cluster-wide lock at commit. If enqueue rates make this
  visible in the load test, `JOBS_NOTIFY=false` turns it off and workers rely on the poll
  (default 1 s, §11.2). The poll is always on; NOTIFY only lowers latency.

### 6.3 De-duplication

With `unique_key`, the insert is `INSERT … ON CONFLICT (kind, unique_key) WHERE … DO NOTHING
RETURNING id`; on conflict the existing id is selected and `Duplicate(id)` returned. It never
raises, so it never aborts the caller's transaction (unlike apalis). Uniqueness covers
`available` and `running` jobs; a finished job does not block a new one.
Optional `replace: true` updates the waiting duplicate's `args` and `run_at` (debounce);
it does nothing if the duplicate is already running.

## 7. Claiming

### 7.1 SQL

One short transaction per claim, per queue:

```sql
UPDATE jobs SET state = 'running', attempt = attempt + 1, locked_by = $process,
       locked_until = now() + make_interval(secs => lease_s),
       attempt_started_at = now()
WHERE id IN (
  SELECT id FROM jobs
  WHERE state = 'available' AND queue = $queue AND run_at <= now()
    AND kind = ANY($known_kinds) AND concurrency_key IS NULL
  ORDER BY priority, run_at, id
  LIMIT $free_slots
  FOR UPDATE SKIP LOCKED)
RETURNING *;
```

- Many processes claim in parallel without blocking each other or taking the same row.
- A job **never runs inside a transaction**: the claim commits before the handler starts,
  so no lock or connection is held while a job runs.
- `LIMIT` is the number of free handler slots for that queue in this process, so a process
  never claims jobs it cannot start (apalis's buffered-claims problem).
- Paused queues are not claimed (the worker reads `job_queue_pauses` every poll; cheap).
- Jobs with a `concurrency_key` are claimed by a separate statement, **one row per
  statement**; the unique index `jobs_one_running_per_key` makes a second running job with
  the same key impossible. A unique violation means another worker won: the claimer skips
  that key for this poll.

### 7.2 Queue-wide limits

A queue with `max_running` (global across all processes; e.g. imports = 2) claims under
`pg_advisory_xact_lock(hashtext('jobs.queue.' || $queue))`, counts `state = 'running'`
jobs of the queue, and claims at most `max_running - running`. Only limited queues take the
lock; they are low-throughput by nature. A slot frees when the job finishes or its lease is
reaped, so a crashed job cannot hold a slot forever and a long job cannot be overtaken
(unlike Solid Queue's time-based semaphores).

## 8. Leases, heartbeats and the process registry

### 8.1 Start and stop

On start a worker inserts its `job_processes` row and keeps the id. On clean stop it deletes
the row after releasing its jobs (§13).

### 8.2 Heartbeat (one task per process, never a job's task)

Every `lease/3` seconds (default 20 s), on a dedicated Tokio task with its own pooled
connection, one transaction:

```sql
UPDATE job_processes SET last_heartbeat_at = now() WHERE id = $me RETURNING id;   -- (a)
UPDATE jobs SET locked_until = now() + make_interval(secs => lease_s)               -- (b)
WHERE state = 'running' AND locked_by = $me AND id = ANY($my_running_ids)
  AND attempt_started_at + make_interval(secs => deadline_s) > now()
RETURNING id, cancel_requested;
```

- (a) returns nothing ⇒ this process was pruned (it was presumed dead): **self-fence**. Stop
  claiming, cancel all handlers, and exit with an error so the supervisor (systemd, Docker
  restart policy) restarts it.
- (b) A running id missing from the result has lost its lease (reaped and maybe re-claimed
  elsewhere) or passed its deadline: cancel that handler's token. Rows with
  `cancel_requested` cancel their handlers (§10).
- If the heartbeat itself fails (database unreachable) for longer than one lease, the worker
  cancels all handlers, because another worker may legitimately take them over.

The heartbeat runs on the multi-threaded runtime, separate from handlers; CPU-heavy handlers
must use `spawn_blocking` (§11.3) so they cannot starve it.

### 8.3 Fencing (stale workers cannot write)

Every write a worker makes for a job after claiming it carries the claim's identity:

```sql
... WHERE id = $id AND state = 'running' AND locked_by = $me AND attempt = $attempt
```

This applies to completion, failure, retry scheduling, progress and release. Zero rows
updated means the lease was lost: the result is dropped and logged
(`jobs.fence_rejected`). The attempt number makes the token unique even if the same
process re-claims the same job later.

### 8.4 Deadlines

Each kind sets `deadline` (default 15 min; webhooks 60 s; reports and imports in
sub-project 2 set hours). The handler runs under `tokio::time::timeout(deadline)`, and the
heartbeat stops extending a job past its deadline (8.2 b), so even a process whose handler
ignores cancellation loses the job within one lease. Outcome: cause `deadline_exceeded`,
counted as a normal failed attempt (retried if attempts remain).

### 8.5 Maintenance leader

Exactly one process at a time runs maintenance: it holds a session-level
`pg_try_advisory_lock(jobs.maintenance)` on a dedicated connection (lost automatically if
that connection dies; another process takes over at its next try, every 10 s). The leader
runs:

| Task | Every | What |
|---|---|---|
| Reaper | 5 s | `running` jobs with `locked_until < now()` (§9.3) |
| Pruner | 30 s | processes with `last_heartbeat_at < now() - 3 × lease`: set `locked_until = now()` on their running jobs (reapable at once, cause `process_lost`), then delete the row |
| Recurring | 1 s | enqueue due ticks (§14) |
| Retention | 60 s | batched deletes (§16) |
| Gauges | 15 s | aggregate queries for metrics (§17.2) |

## 9. Outcomes, retries and crashes

### 9.1 Outcome of an attempt

| Handler result | New state | Notes |
|---|---|---|
| `Ok(())` | `succeeded`, `finished_at = now()` | |
| `Retry` / `anyhow` error / panic / `deadline_exceeded` | `available`, `run_at = now() + backoff(attempt)` if `attempt < max_attempts`, else `failed` | panic is caught by the task's `JoinError`, never kills the worker |
| `RetryAfter(d)` | as above with `run_at = now() + max(d, backoff)` | |
| `Permanent` | `failed` | |
| `Cancelled` (after `cancel_requested`) | `cancelled` | |

Every non-success appends `{attempt, at, cause, message}` to `errors` (last 10 kept).

### 9.2 Backoff

`backoff(n) = min(cap, base × 2^(n-1))` with full jitter (uniform 0…value), base 5 s, cap
1 h by default; per kind overrides. Jitter prevents retry storms after an outage.

### 9.3 Crashes (lease expiry)

The reaper turns each expired lease into an attempt outcome with a cause:

- `process_lost` if the holder's registry row is gone or its heartbeat is older than one lease,
- `lease_expired` otherwise (the process heartbeats but stopped renewing this job: it passed
  its deadline, or the process is frozen or partitioned between beats).

`crashes += 1`. If `crashes >= max_crashes` (default 2) or `attempt >= max_attempts`, the job
becomes `failed`; otherwise `available` with backoff. A job that keeps killing its worker
(out of memory, segfault in a C library) therefore fails after two crashes instead of
looping (apalis, fang) or failing on the first unrelated restart (Solid Queue).

### 9.4 Unknown kind

Never claimed by a process that does not know it (§7.1). `jobs status` lists kinds that no
live process knows, so a missing deploy is visible.

## 10. Cancellation and progress

- **Cancel:** `jobs cancel <id>` (or `jobs::cancel(&pool, id)` from the app) sets
  `cancel_requested`. A waiting job becomes `cancelled` at once. A running job's handler sees
  `ctx.cancelled()` fire within one heartbeat (≤ 20 s), stops, and returns
  `JobError::Cancelled`; if it does not stop, the deadline still applies.
- **Progress:** `ctx.set_progress(json)` writes at most once per second (coalesced),
  fenced (§8.3). Shown in `jobs show` and `jobs list --running`.

## 11. Worker runtime

### 11.1 Queues and parallelism

Config per process (`JOBS_QUEUES="email:8,webhooks:16,default:4"`): one claim loop per queue,
each with a semaphore of N handler slots. Each claimed job runs on its own Tokio task, so
many slow I/O jobs (SMTP, HTTP) wait concurrently on few threads. Queue isolation means a
backlog of webhooks cannot delay email.

### 11.2 Wake-ups

One `LISTEN jobs` connection per process wakes the matching queue loop. Every loop also polls
every `JOBS_POLL` (default 1 s, ±10 % jitter) and right after finishing a job if the last
claim was full. After a listener disconnect, it reconnects with backoff and polls meanwhile.

### 11.3 CPU-heavy jobs

Handlers that compute (reports, parsing) do it inside `spawn_blocking` or on a dedicated
`rayon` pool, never on the async workers. The crate documents this and the heartbeat design
does not depend on it (separate task), but starving the runtime would still delay heartbeats.

### 11.4 Connections

The worker uses its own pool (default `max(4, total slots / 4)`, `JOBS_DB_POOL`) plus one
listener and one leader connection. Handlers use the same pool via `ctx`. Worker connections set
`statement_timeout = 30s`; a handler that needs longer queries (reports) raises it with
`SET LOCAL` inside its own transaction.

### 11.5 Database outage

Claim, heartbeat and completion errors are logged and retried with backoff (1 s → 30 s);
the process never exits for an outage. Completions that fail are retried in memory until
the fence rejects them or they succeed; if the lease has expired meanwhile, the job was
re-run elsewhere (at-least-once).

## 12. Limits

### 12.1 Per-process concurrency

The per-queue slots of §11.1.

### 12.2 Per-key concurrency

`concurrency_key` gives at most **one** running job per key across all processes, enforced
by the unique partial index (§7.1). Use: one import per member, one webhook delivery per
endpoint+entity when order matters. Limits above 1 per key are out of scope.

### 12.3 Rate limits

Per-process token buckets keyed by a handler-chosen string (`ctx.rate_limit("webhook:host",
rate)`), which waits for a token. The effective global rate is `rate × processes`; documented.
A handler can also return `RetryAfter` to push a job back.

## 13. Graceful shutdown

On SIGTERM/SIGINT:

1. Stop claiming; mark `job_processes.stopping = true`.
2. Wait for running handlers up to `JOBS_SHUTDOWN_TIMEOUT` (default 25 s). Heartbeats
   continue meanwhile. Compose sets `stop_grace_period: 40s`, longer than Docker's default
   10 s, so the steps below finish before Docker sends SIGKILL.
3. Cancel the remaining handlers' tokens and wait 5 s more.
4. Release jobs whose handlers stopped without finishing: back to `available` with
   `attempt - 1` (a shutdown is not the job's fault), cause `released_on_shutdown`, fenced.
5. Jobs whose handlers still did not stop keep their leases; the process deletes its
   registry row and exits. The reaper recovers them after the lease (counted as a crash, as
   the handler ignored cancellation).

A second signal skips to step 5.

## 14. Recurring jobs

Declared in code (`registry.recurring("retention", "*/1 * * * *", || JobPayload)`) with a
cron expression (UTC). The leader computes due ticks from database time and, per tick, in one
transaction inserts `job_recurring_runs (task, run_at)` with `ON CONFLICT DO NOTHING` and,
only if inserted, enqueues the job. Two leaders (during a hand-over) cannot enqueue the same
tick twice. Missed ticks (all workers down) are not replayed: only the latest due tick runs.

## 15. The first jobs

### 15.1 Enqueue points

`loans/postgres.rs` `create_checked` and `mark_returned` enqueue, inside their existing
transactions and just before `commit`:

- `LoanEmail { loan_id, event }` on the `email` queue, and
- one `Webhook { endpoint, event_id, event }` per configured endpoint on the `webhooks`
  queue (endpoints fail and retry independently).

The repos get an `Option<Enqueuer>`; with none configured (jobs disabled, or SQLite) nothing
is enqueued and behaviour is exactly as today. The API's responses do not change.

### 15.2 `loan_email.v1`

- Loads the loan, member and book by id **at run time** (the email reflects the current
  state; if the loan was deleted, the job succeeds with "nothing to send").
- Sends via SMTP (`lettre`, async, STARTTLS when configured, 30 s timeout) to the member's
  address, plain-text body.
- `Message-ID: <loan_email.v1.{job id}@{JOBS_EMAIL_DOMAIN}>`, stable across attempts, so
  receiving systems can de-duplicate a resend after a crash. SMTP offers nothing stronger;
  a duplicate email after a crash between "250 OK" and the completion write is the accepted
  at-least-once cost.
- Errors: connection/timeout/4xx SMTP replies → `Retry`; 5xx permanent replies (bad
  mailbox) → `Permanent`.

### 15.3 `webhook.v1`

- `POST` JSON `{id, type: "loan.borrowed"|"loan.returned", occurred_at, data: {loan}}` to the
  endpoint, with headers per the Standard Webhooks spec: `webhook-id` (the event id,
  identical across retries, for receiver de-duplication), `webhook-timestamp`,
  `webhook-signature` (`v1,` + base64 HMAC-SHA256 of `id.timestamp.body` with the endpoint
  secret).
- `reqwest` with rustls, connect timeout 5 s, total 15 s, **no redirects**, response body
  read up to 64 KB and discarded.
- 2xx → success. 408, 429 (honouring `Retry-After`), 5xx, timeouts, connection errors →
  retry. Other 4xx → `Permanent`.
- Endpoints and secrets come only from operator config (`JOBS_WEBHOOK_ENDPOINTS` and
  `JOBS_WEBHOOK_SECRET_<n>`), never from API input, so there is no SSRF surface in this
  sub-project. Payload data is captured at enqueue (an event is a fact at a point in time),
  unlike the email.
- No ordering guarantee between events; `occurred_at` lets receivers order them.

## 16. Retention

Batched deletes by the leader (`DELETE … WHERE id IN (SELECT id … LIMIT 1000)`, repeated
with short pauses): `succeeded` after `JOBS_KEEP_SUCCEEDED` (default 1 day), `failed` and
`cancelled` after `JOBS_KEEP_FAILED` (default 14 days). Recurring runs after 7 days. Job
args may hold personal data (a member id resolves to an email), so retention also bounds
how long it is kept.

## 17. Observability and operations

### 17.1 Events (logs)

A `tracing` event for every transition, with `job_id`, `kind`, `queue`, `attempt`:
`jobs.enqueued`, `jobs.duplicate`, `jobs.claimed`, `jobs.succeeded`, `jobs.retry_scheduled`,
`jobs.failed`, `jobs.cancelled`, `jobs.released`, `jobs.lease_lost`, `jobs.fence_rejected`,
`jobs.reaped` (with cause), `process.started`, `process.stopping`, `process.pruned`,
`process.self_fenced`, `leader.acquired`, `leader.lost`, `recurring.enqueued`. Handler spans
are children of the enqueuing request's trace (`trace` column).

### 17.2 Metrics (`/metrics` on the worker)

Per process (counters/histograms): `jobs_claimed_total`, `jobs_completed_total{kind,outcome}`,
`jobs_duration_seconds{kind}`, `jobs_wait_seconds{kind}` (claim time − run_at),
`jobs_fence_rejected_total`, `jobs_lease_lost_total`, `jobs_db_errors_total{op}`,
`jobs_running{queue}`, `jobs_slots{queue}`.

Leader-only gauges (one aggregate query every 15 s, counts capped at 100,000):
`jobs_available{queue}`, `jobs_scheduled{queue}` (run_at in the future),
`jobs_oldest_available_seconds{queue}` (queue latency), `jobs_running_total{queue}`,
`jobs_failed_recent{kind}` (last 24 h), `jobs_processes_live`.

Suggested alerts: oldest available > 5 min; failed rate up; no live process; lease-lost or
fence-rejected non-zero.

### 17.3 Health

`/healthz`: the process loop is alive. `/readyz`: database reachable, registry row present,
heartbeat within one lease.

### 17.4 Ops CLI: `library-web jobs …`

| Command | Does |
|---|---|
| `status` | Per queue: available / scheduled / running / failed (24 h), oldest age, paused?; live processes; kinds unknown to every live process. |
| `list [--state s] [--queue q] [--kind k] [--limit n]` | Newest first, capped. |
| `show <id>` | Everything, including errors and progress. |
| `retry <id>` / `retry --failed [--kind k] [--since t]` | Failed or cancelled → available, attempts and crashes reset. |
| `discard <id>` | Waiting → cancelled. |
| `cancel <id>` | §10. |
| `pause <queue> [--reason r]` / `resume <queue>` | §4.3. |
| `processes` | Registry with heartbeat age; marks stale ones. |
| `prune --dry-run` | What retention would delete now. |

## 18. Configuration (environment)

`JOBS_ENABLED` (default false: no enqueue, no migrations of the jobs schema on `serve`),
`JOBS_EMBEDDED`, `JOBS_QUEUES`, `JOBS_POLL`, `JOBS_NOTIFY`, `JOBS_DB_POOL`,
`JOBS_LEASE` (default 60 s), `JOBS_SHUTDOWN_TIMEOUT`, `JOBS_KEEP_SUCCEEDED`,
`JOBS_KEEP_FAILED`, `JOBS_METRICS_ADDR` (default `0.0.0.0:9300`), `JOBS_SMTP_URL`,
`JOBS_EMAIL_FROM`, `JOBS_EMAIL_DOMAIN`, `JOBS_WEBHOOK_ENDPOINTS`,
`JOBS_WEBHOOK_SECRET_<n>`. Secrets are never logged and never stored in job args.

## 19. Deployment

- `deploy/compose`: a `worker` service (same image, `command: worker`,
  `stop_grace_period: 40s`, restart `unless-stopped`) next to `web`. For the 2-VM layout it
  runs on the app VM; it can move to its own VM unchanged.
- Migrations are expand-only (decision 0003). Enabling jobs is `JOBS_ENABLED=true` on both
  `serve` and `worker`; disabling it leaves waiting jobs in the table, untouched.
- Rolling deploy: a new kind version is registered by new workers first; old workers never
  claim it (§7.1). Remove an old version only after `jobs status` shows none waiting.
- Runbook additions (`docs/RUNBOOK.md`): stuck queue, failed jobs, poison job, paused queue,
  replaying after an SMTP outage, adding a webhook endpoint.

## 20. Testing

Integration tests run against real Postgres (`TEST_POSTGRES_URL`, the existing
`scripts/test-postgres.sh`), with fast settings (lease 2 s, poll 100 ms). Each must pass
repeatedly (run the suite 20× in CI-like conditions before merge).

| Area | Test |
|---|---|
| Outbox | Rollback ⇒ no job; commit ⇒ job; NOTIFY not seen before commit. |
| De-dup | 16 concurrent enqueues of one unique key ⇒ 1 job, 15 `Duplicate`, no error, caller tx intact. `replace` updates a waiting job only. |
| Claiming | 4 workers × 8 slots, 5,000 no-op jobs ⇒ each runs exactly once (no crashes injected). |
| Unknown kind | A process without the kind never claims it. |
| Crash | A worker is dropped mid-job without completing (simulated `kill -9`) ⇒ job re-run after lease, `crashes = 1`, cause recorded. Also a real child-process test: spawn `library-web worker`, SIGKILL, assert recovery. |
| Poison | A job that "crashes" every time fails after `max_crashes`. |
| Fencing | Worker A pauses past its lease; B re-claims and finishes; A's completion and progress writes change 0 rows. |
| Self-fence | Delete a live process's registry row ⇒ it cancels handlers and exits non-zero. |
| Deadline | A handler that ignores cancellation loses its lease after the deadline; the job is retried elsewhere. |
| Cancel | Waiting job ⇒ cancelled at once; running job's token fires within one heartbeat. |
| Retries | `Retry`, `RetryAfter`, `Permanent`, panic ⇒ correct states, `run_at` within the jitter window, errors appended and capped at 10. |
| Limits | Queue `max_running = 2` with 4 processes: never more than 2 running (sampled every 10 ms). Concurrency key: never two running with one key. |
| Shutdown | SIGTERM mid-job: finished jobs complete; stopped ones are released with `attempt` unchanged; stubborn ones are reaped later. |
| Leader | Kill the leader's connection ⇒ another process becomes leader within 15 s; reaper keeps working. |
| Recurring | Three processes, forced leader hand-overs ⇒ each tick enqueued once. |
| Pause | A paused queue is not claimed; running jobs finish. |
| Retention | Only rows past their window are deleted; batches bounded. |
| DB outage | Stop Postgres for 20 s mid-run: no worker exits; afterwards every job ends in a terminal state exactly as the guarantees say. |
| Email | Against Mailpit (container in tests): message delivered with the stable Message-ID; 5xx ⇒ permanent; 4xx ⇒ retry. |
| Webhook | Against an in-test axum server: signature verifies; 429 with `Retry-After` honoured; redirect not followed; 404 permanent; 500 retried. |
| App | Borrow/return enqueue exactly one email + one webhook per endpoint in the same transaction; a failed borrow (409) enqueues nothing; contract test still 402/402 with jobs on. |

Plus unit tests for backoff, cron tick computation, config parsing, the registry, and
clippy/fmt with and without the `postgres` feature.

## 21. Acceptance

- All tests in §20 pass; `cargo test` with and without `--features postgres`; clippy clean.
- Request-path cost: with jobs on, borrow/return p95 latency in the load test rises by less
  than 1 ms at equal load (measured in sub-project 5; a smoke measurement here).
- Throughput smoke test on the 2-core box: ≥ 1,000 no-op jobs/s with one worker process.
- Docs: `docs/JOBS.md` (concepts, guarantees, writing a job, operating), runbook entries,
  `docs/ARCHITECTURE.md` module map, a decision record `docs/decisions/0007-own-job-queue.md`.

## 22. Changes made while planning and implementing

Decided while planning:

- Recurring tasks run on a fixed interval (`every`, at least 1 s), not cron. A tick is a multiple
  of the interval since the epoch, in database time, still de-duplicated on `(task, run_at)`.
- No `enqueue_many`: callers loop inside their transaction (the loan events do).
- No `trace` column.
- Environment names carry units: `JOBS_POLL_MS`, `JOBS_SHUTDOWN_TIMEOUT_S`,
  `JOBS_KEEP_SUCCEEDED_H`, `JOBS_KEEP_FAILED_H`, `JOBS_SMTP_TIMEOUT_S`. The lease is per kind in
  code (`JobOptions::lease`, default 60 s), not an environment variable; the heartbeat is 20 s.
- The jobs migration table is `_jobs_migrations`, set with sqlx's
  `Migrator::dangerous_set_table_name`, not `sqlx.toml` (§3.3).
- Emails are enqueued only when `JOBS_SMTP_URL` is set, webhooks only for configured endpoints.
  A worker registers only the kinds it has settings for, so their jobs wait, and `jobs status`
  names them, rather than fail.
- The real-SIGKILL test re-runs the crate's test binary as the child worker
  (`crates/jobs/tests/child_process.rs`). The `library-web` binary itself is tested for start,
  readiness and SIGTERM (`apps/web/tests/jobs_worker.rs`).
- The outage test pauses a dedicated Postgres container (`docker pause`) and is `#[ignore]`d in
  CI.
- Alerts `JobsQueueStalled`, `JobsFailing`, `JobsNoLeader` join `deploy/monitoring/alerts.yml`,
  with promtool tests, and the runbook gets matching sections.

Decided while implementing (the code is the authority where §3 to §17 differ):

Queue core (`crates/jobs`):

- Enqueue with `replace: true` also updates `concurrency_key`. On a row waiting out a retry
  (`attempt > 0`) it keeps the later `run_at` (`GREATEST`), so a replace cannot shorten a backoff.
- A keyed claim takes `pg_try_advisory_xact_lock(hashtextextended('jobs.key.' || key, 0))` and
  skips the key when the lock is taken; the unique-violation skip of §7.1 deadlocked. The unique
  index `jobs_one_running_per_key` stays as a backstop. (§7.1 and §7.2 name `hashtext` and the
  unique violation; that text is stale.)
- Retry delays are capped at 7 days (`MAX_RETRY_DELAY`), so a remote `Retry-After` above the
  backoff cap is honoured up to that. Stored error text has NUL replaced by U+FFFD and is cut to
  2,000 characters.
- `release` on shutdown gives the attempt back (`attempt - 1`), as §13 step 4 says; the line
  "released with attempt unchanged" in the §20 test table is stale.
- The executor builds the handler (custom `Deserialize`, the synchronous part of `perform`)
  inside the isolating spawn, so a panic there is recorded as `panic`. A deadline always attempts
  the fenced `deadline_exceeded` write, even after a beat marked the job lease-lost.
- During a database outage the heartbeat cancels handlers at the last good beat plus (shortest
  lease minus heartbeat), one beat before a lease can expire, and checks again at every beat
  while beats fail. Each beat call times out after one heartbeat; progress flushes run in their
  own task. An outage never fences and never exits the process; only a deleted registry row
  self-fences. This replaces §8.2's "for longer than one lease".
- A worker claims nothing while its heartbeat is stale (age above shortest lease minus
  heartbeat). A finished attempt removes only its own entry from the running set. Shutdown
  bounds: helpers join within 5 s, handlers within `JOBS_SHUTDOWN_TIMEOUT_S`, then `cancel_grace`;
  registry writes are bounded to 5 s each.
- The leader holds its lock on one dedicated connection (one beyond the pool) and checks it
  every `leader_retry` with a timeout, so a half-open connection ends leadership. Every
  maintenance statement is bounded to 30 s. Election is tried on a pooled connection, which is
  detached only on a win; a cancelled attempt never returns a lock-holding connection to the pool.
- The pruner removes silent processes only after the database has been healthy (no failing
  maintenance step) for `prune_after`, counted from leadership start or the last failure; after
  an outage longer than `prune_after` it no longer deletes every live process. A step that fails
  for good starves pruning, with a warning after 3 × `prune_after`.
- The reaper reports a job whose process froze past its deadline as `lease_expired` (§9.3), not
  `deadline_exceeded` as counted in §8.4: the worker-side timeout covers cooperative handlers, so
  only frozen processes reach the reaper.
- Recurring ticks are enforced by the database: the tick is computed from `now()` in SQL, and the
  claim row and the insert share one transaction. After an outage only the latest tick runs.
- `/readyz` calls a heartbeat stale after 3 × heartbeat (§17.3 says "within one lease"; validation
  keeps 3 × heartbeat at most the shortest lease).
- `http::serve`'s graceful drain has no upper bound of its own; callers bound it (crate README,
  "Bound the HTTP drain").
- `ops::retry` returns false, and `retry_failed` skips the row, when another waiting or running
  job holds the unique key; of several failed rows with one key only the newest moves.

The web app (`apps/web`):

- Worker pools (`jobs::connect`) set `statement_timeout = 30s` on each connection (§11.4). The
  `worker` command runs the app's and the jobs migrations on that pool, so the deploy rule is: `serve`
  first, the worker after it.
- `JOBS_EMBEDDED=true` without `JOBS_ENABLED=true` is a configuration error (it used to be
  ignored).
- If the embedded worker fences itself or fails, `serve` shuts down and exits non-zero so the
  supervisor restarts it. Embedded mode cannot promise a clean exit within a 40 s grace period
  (HTTP drain, then the worker's stop of up to `JOBS_SHUTDOWN_TIMEOUT_S` + 5 s + 15 s).
- The standalone worker's worst-case shutdown is about 50 s with the database dead, and about 35 s
  with a stuck handler and a healthy database; the metrics server's drain is bounded to 5 s.
- Webhook URLs never appear in configuration errors, `Debug` output, job arguments, unique keys or
  job errors. A job carries `endpoint_id`: `scheme://host[:port]`, `#`, and 12 hex characters of
  the URL's SHA-256. Changing an endpoint's URL makes its waiting jobs fail as "no longer
  configured".
- Loan email: lettre connection pooling is off (a send cancelled by the timeout must not leave a
  desynchronised connection behind). The whole send is bounded by `JOBS_SMTP_TIMEOUT_S`, since
  lettre's own timeout covers only the connect. Job errors carry no member address or name and no
  SMTP URL, only the status code.
- Known gap: a second signal does not skip to step 5 of §13; the worker keeps waiting out its
  shutdown timeouts.

Final review fixes (the code is the authority where §4 to §19 differ):

- Webhook jobs use `max_attempts` 10 like emails (the kind keeps `JobOptions`' default), not 15
  as §4.1 says. A kind's `max_attempts` is validated to 1 to 1000, so retry arithmetic cannot
  overflow; the reaper clamps its backoff exponent too.
- §17.2 `jobs_running_total{queue}` is `jobs_running_all{queue}` (leader), and `jobs_leader` (1 on
  the leader, 0 elsewhere, exported by every worker) is added. `jobs_db_errors_total{op}` also
  counts `heartbeat` and `finish`. `jobs_lease_lost_total` counts each lost lease once, where
  the heartbeat cancels the handler, not when the handler returns. Every `jobs_completed_total`
  series (registered kind × outcome) exists at 0 from worker start.
- The log events are those of §17.1 with these changes: `jobs.failed` is warn, `jobs.retry_scheduled`,
  `jobs.cancelled` and `jobs.released` info, `jobs.succeeded`, `jobs.enqueued`, `jobs.duplicate`,
  `jobs.claimed` and `jobs.finished` debug; there is no `trace` field; `jobs.bad_args` and
  `jobs.lease_expired` are added. Arguments are never logged, and `jobs.bad_args` carries no decode message.
- §17.2's suggested alerts are `JobsQueueStalled`, `JobsFailing` (the workers' failed outcomes,
  or a rise of the leader's `jobs_failed_recent`, which also sees jobs failed by the reaper),
  `JobsNoLeader`, `JobsWorkerDown` ("no live process": a `jobs-worker` scrape target down for
  2 minutes) and `JobsLeaseLost` ("lease-lost or fence-rejected non-zero": either counter rose in
  15 minutes). The last two need the `jobs-worker` scrape job, which is commented out in
  `deploy/monitoring/prometheus.yml`.
- §19: the `worker` compose service exists only in the load-test compose
  (`deploy/compose/local/loadtest.web-pg.yml`, profile `jobs`); `deploy/compose` has none, because
  the production stack runs SQLite.
- A cancellation request now sticks: a retryable failure, or a reap, of a job with
  `cancel_requested` ends it `cancelled` instead of requeueing it (the claim is unchanged).
- The leader connection sets `tcp_keepalives_idle = 10`, `tcp_keepalives_interval = 5` and
  `tcp_keepalives_count = 3` once the lock is granted, so a leader whose host dies without
  closing the connection is replaced in about 30 s rather than after the server's keepalive
  default (§8.5's "within 10 s" holds for a closed connection or a stopped process).
- New validations: `prune_after` at least the shortest lease minus a heartbeat; no zero
  maintenance interval (tokio's `interval` panics on one) while maintenance is enabled; the
  `heartbeat × 3` checks cannot overflow. A claim and a heartbeat renewal give a job a lease of
  at least the worker's shortest lease, so a row enqueued with a shorter one is not reaped under
  a live handler. `ListFilter::default()` lists 50.
- App: `JOBS_WEBHOOK_ENDPOINTS` refuses a URL listed twice (naming the two positions); webhook
  transport errors read "timed out", "connection failed" or "request failed", and a request that
  cannot be built is permanent; `JOBS_SMTP_URL` with credentials must use `smtps://` or
  `?tls=required` (or `wrapper`).

Measured (§21):

- Throughput smoke: about 16,700 no-op jobs/s with one worker, 64 slots and 2 cores (`taskset`
  6,7), about 19,000 on 4 cores, with Postgres on the same 16-thread machine.

## 23. Using the crate from axum, tokio and other frameworks

The crate must be usable from any tokio-based Rust web stack, not only this app. Guide:
`crates/jobs/README.md`.

- **No framework in the core.** `Enqueuer` takes a `&mut PgConnection`, `Worker::run` is a plain
  future, `Metrics::encode()` returns text, and `ready(&Health, &PgPool, stale_after)` answers
  readiness. axum appears only behind the default feature `http` (`http::router`, `http::serve`,
  `Worker::router()`); `default-features = false` drops the dependency.
- **tokio is required** (sqlx's Postgres driver and the worker's tasks use it). A multi-threaded
  runtime is recommended: on `current_thread`, a blocking handler stalls the heartbeat and the
  process loses its leases. Handlers are `Send + 'static`.
- **Three ways to run a worker,** all documented with code: a separate process (the default
  here), `tokio::spawn` inside the server sharing one `CancellationToken` with axum's graceful
  shutdown, or a runtime of its own on an OS thread (for actix-web, whose runtimes are
  single-threaded, and for CPU-heavy jobs beside an API). The worker's pool is created inside the
  runtime that uses it.
- **axum:** `Enqueuer` in the state via `FromRef`; enqueue on `&mut *tx` in the handler or in the
  repository just before `commit`; `EnqueueError` maps to a 500 like any database error; the
  internal router is served on a separate address, or nested behind admin auth.
- **Other frameworks** (actix-web, Rocket, Poem, Salvo, warp, hyper, Loco): the same three pieces
  through their own state and handler mechanisms. async-std and smol are not supported.
- **`examples/axum_app.rs`** is a complete, compiled example (sign-up handler that enqueues in its
  transaction, embedded worker, internal router, shared shutdown), so the README's main snippet
  cannot drift from the API. Other frameworks get uncompiled sketches.

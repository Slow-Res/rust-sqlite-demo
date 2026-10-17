# Background jobs

Borrowing or returning a book sends the member an email and tells outside systems through signed
webhooks. Neither may slow the request or fail it when the mail server or a receiver is down, and
neither may happen for a loan change that rolled back. So the request only writes a row in the
`jobs` table, in the same transaction as the loan, and a separate worker process does the sending.

Postgres only. The SQLite deployment has no jobs. The queue is the generic crate
[`library-jobs`](../crates/jobs/README.md) (how to use it from axum, tokio or another framework is
in its README). The app's jobs live in `apps/web/src/jobs/`. The design is in
[the spec](superpowers/specs/2026-10-14-background-jobs-design.md); why we built the crate is in
[decision 0007](decisions/0007-own-job-queue.md).

## Contents

- [Processes](#processes)
- [One borrow, step by step](#one-borrow-step-by-step)
- [The jobs](#the-jobs)
- [Guarantees](#guarantees)
- [Configuration](#configuration)
- [Operating](#operating)
- [Adding a job](#adding-a-job)
- [Testing](#testing)

## Processes

| Process | Command | Runs job code? | Port |
|---|---|---|---|
| API | `library-web serve` | No. It only enqueues. | 4000 |
| Worker | `library-web worker` | Yes | 9300: `/metrics`, `/healthz`, `/readyz` |
| Embedded | `library-web serve` with `JOBS_EMBEDDED=true` | Yes, inside the API process | none |

Same image, same binary. The separate worker is the production default: a slow or crashing job
cannot slow or crash the API, a worker restart does not drop requests, and workers scale on their
own. Embedded mode is for development and very small deployments.

Several workers can run at once, on one VM or several. They agree through the database only:

- **Claims** use `FOR UPDATE SKIP LOCKED`, so two workers never take the same job.
- **Leases**: a running job belongs to one worker until `locked_until`, 60 s ahead (the lease is
  per kind, `JobOptions::lease`). Each worker renews the leases of all its jobs every 20 s, in one
  transaction of two statements: it touches its registry row, then renews the leases (the
  heartbeat).
- **The registry** (`job_processes`) has one row per worker, updated by the same heartbeat.
- **The leader** is whichever worker holds a Postgres advisory lock. It re-queues jobs whose lease
  ran out (every 5 s), deletes workers silent for 3 minutes (every 30 s), enqueues recurring jobs,
  deletes old jobs (succeeded after 24 h, failed after 14 days) and publishes the queue gauges.
  It holds the lock on one dedicated connection (one beyond `JOBS_DB_POOL`). If that connection
  closes, or the leader's process exits, another worker takes over within about 10 s. If the
  leader's host dies without closing it, Postgres frees the lock only when it notices the dead
  client: the leader connection sets TCP keepalives of 10 s idle and 3 probes 5 s apart, so
  that is up to about 30 s (the server default would be about 2 hours). See
  [the runbook](RUNBOOK.md#jobs-a-queue-is-not-moving) to find and end a stuck holder. A leader removes silent workers from the registry
  only after the database has answered every maintenance step for 3 minutes, so after an outage it
  does not delete every live worker that could not beat meanwhile.
- **A database outage** never fences a worker and never ends the process. The worker stops
  claiming once its last heartbeat is older than a lease minus a heartbeat (40 s), and cancels its
  handlers one heartbeat before their lease could expire, so another worker cannot run the same
  job while this one still does. Only a deleted registry row (the leader removed this worker as
  dead) makes a worker fence itself and exit.

## One borrow, step by step

| # | What happens | Where |
|---|---|---|
| 1 | `POST /api/loans`. The repository locks the book and member rows, checks copies and limits, inserts the loan. | `apps/web/src/loans/postgres.rs` `create_checked` |
| 2 | Still in that transaction: one `loan_email.v1` job and one `webhook.v1` job per configured endpoint are inserted. | `apps/web/src/jobs/events.rs` `LoanEvents::record` |
| 3 | `COMMIT`. The loan and its jobs become visible together. Postgres delivers `NOTIFY jobs, 'email'` (and `'webhooks'`) at commit, never before. The response is sent. | `crates/jobs/src/enqueue.rs` |
| 4 | An idle worker's LISTEN connection wakes its `email` loop, which claims the job and sets its lease. Without the notification, the loop's 1 s poll finds it. | `crates/jobs/src/worker.rs`, `store/claim.rs` |
| 5 | The handler reads the loan, member and book as they are now, and sends the email over SMTP. | `apps/web/src/jobs/loan_email.rs` |
| 6 | The worker writes `succeeded` only if it still holds the job (same worker, same attempt). | `crates/jobs/src/store/finish.rs` |
| 7 | 24 h later the leader deletes the row. | `crates/jobs/src/store/maintenance.rs` |

A borrow that is refused (no copy free, limit reached) rolls back and enqueues nothing.

## The jobs

| Kind | Queue | Enqueued by | Does | Retries |
|---|---|---|---|---|
| `loan_email.v1` | `email` | borrow and return, when `JOBS_SMTP_URL` is set | Emails the member, plain text. `Message-ID: <loan_email.v1.<job id>@JOBS_EMAIL_DOMAIN>`, the same on every attempt. Skips a loan that is gone, and a borrow email for a loan already returned. | Connection errors, timeouts and 4xx replies retry; 5xx replies and invalid addresses fail at once. |
| `webhook.v1` | `webhooks` | borrow and return, once per endpoint in `JOBS_WEBHOOK_ENDPOINTS` | `POST`s `{id, type, occurred_at, data: {loan}}` with Standard Webhooks headers: `webhook-id` (`loan.<id>.<borrowed\|returned>`), `webhook-timestamp`, `webhook-signature` (`v1,<base64 HMAC-SHA256>`). Redirects are not followed. The payload is the loan at the time of the event. | 408, 429 (waits for `Retry-After`, up to 1 h), 5xx, timeouts and connection errors retry; other 4xx and redirects fail at once. |

Both retry up to 10 attempts. The delay before retry *n* is random between 0 and 5 s × 2^(n-1)
(full jitter, capped at 1 h), so all nine retries take at most about 43 minutes (longer when a
receiver sends `Retry-After`) and about 21 on average.
A `Retry-After` from a webhook receiver is honoured instead, up to 1 h; no delay ever exceeds 7
days. A job that fails permanently is never retried.

## Guarantees

- **At least once.** A job can run twice if its worker dies after the email or webhook went out
  but before `succeeded` was written. Receivers de-duplicate on the stable `Message-ID` or
  `webhook-id`.
- **Never lost.** Every job ends `succeeded`, `failed` or `cancelled`, and stays visible in
  `jobs list` until retention deletes it.
- **Exactly when the loan commits.** No job without its loan change, no loan change without its
  jobs: if the enqueue fails, the borrow fails (500) and nothing is written.
- **No order.** A return's webhook can arrive before the borrow's. Receivers order by
  `occurred_at`.
- **Crashes are bounded.** A job whose worker dies twice while running it (`max_crashes`) fails,
  so one bad job cannot take down every worker in turn. The leader reports such a job as
  `process_lost` (its worker's registry row is gone or silent) or `lease_expired` (the worker
  froze past its deadline).
- **Handlers cannot hang a worker.** An attempt that outlives its deadline (2 minutes for emails,
  1 minute for webhooks, 15 by default) is cancelled and retried; a panic is recorded as `panic`
  and the worker goes on; arguments that no longer decode fail the job with `bad_args`.

## Configuration

Set on both `serve` and `worker` (they must agree on what is enqueued and what can be run). A
malformed value stops the process at start with a message naming the variable.

| Variable | Default | Meaning |
|---|---|---|
| `JOBS_ENABLED` | `false` | Enqueue on borrow and return, migrate the jobs schema on start, allow `worker`. Needs a Postgres `DATABASE_URL`. |
| `JOBS_EMBEDDED` | `false` | `serve` also runs a worker. Needs `JOBS_ENABLED=true`; without it the process refuses to start. |
| `JOBS_QUEUES` | `email:4,webhooks:8` | `name:slots[:max_running]`: handlers at once per queue in this process, and optionally across all processes |
| `JOBS_POLL_MS` | `1000` | How often an idle queue looks for work when no notification came |
| `JOBS_NOTIFY` | `true` | `NOTIFY` on enqueue and `LISTEN` in workers |
| `JOBS_DB_POOL` | `16` | The worker's own connection pool, never shared with the API's. Every connection has `statement_timeout = 30s`. |
| `JOBS_SHUTDOWN_TIMEOUT_S` | `25` | On SIGTERM, how long running jobs may finish before they are cancelled (5 s more) and released |
| `JOBS_KEEP_SUCCEEDED_H` | `24` | Retention of succeeded jobs |
| `JOBS_KEEP_FAILED_H` | `336` | Retention of failed and cancelled jobs (14 days) |
| `JOBS_METRICS_ADDR` | `0.0.0.0:9300` | The worker's `/metrics`, `/healthz`, `/readyz`. Keep it off the public network. |
| `JOBS_SMTP_URL` | unset | `smtp://host:25`, `smtp://user:pass@host:587?tls=required` (STARTTLS) or `smtps://user:pass@host:465`. A URL with a username and password must use `smtps://` or `?tls=required` (or `?tls=wrapper`); otherwise the process refuses to start. Unset: no emails are enqueued, and the worker does not run `loan_email.v1`. |
| `JOBS_SMTP_TIMEOUT_S` | `30` | Bounds connecting and the whole send of one email; a slower server fails the attempt as retryable |
| `JOBS_EMAIL_FROM` | `library@localhost` | Sender address |
| `JOBS_EMAIL_DOMAIN` | `library.local` | Right-hand side of the `Message-ID` |
| `JOBS_WEBHOOK_ENDPOINTS` | unset | Comma-separated `http(s)` URLs, at most 10, each listed once (a repeated URL stops the process at start, naming the two positions). Connect timeout 5 s, whole request 15 s. |
| `JOBS_WEBHOOK_SECRET_<n>` | – | Secret of the n-th endpoint (1-based): `whsec_` and the base64 of a 24 to 64 byte key |

Generate a webhook secret with `echo "whsec_$(openssl rand -base64 32)"`. Secrets, webhook URLs and
the SMTP URL are never logged, never put in an error and never stored in job arguments: a webhook
job names its endpoint by `endpoint_id` (`scheme://host[:port]#` and 12 hex characters of the
URL's SHA-256). Changing an endpoint's URL therefore makes its waiting jobs fail as "no longer
configured". Job errors of emails carry no member name or address, only the SMTP status code.

## Operating

### Commands

All run with the worker's environment (`DATABASE_URL`), e.g.
`docker compose exec worker library-web jobs status`.

| Command | Does |
|---|---|
| `jobs status` | Per queue: available, scheduled, running, failed in 24 h, oldest wait in seconds, paused. The workers and their heartbeat age. A warning for kinds no live worker can run. |
| `jobs list [--state s] [--queue q] [--kind k] [--limit n]` | Newest first, with the last error |
| `jobs show <id>` | Everything: arguments, errors (up to 10, 2,000 characters each), progress, lease |
| `jobs retry <id>` | A failed or cancelled job runs again, attempts reset. Refused when another waiting or running job holds its unique key. |
| `jobs retry --failed [--kind k] [--since 2h]` | The same, in bulk. Of several failed rows with one key only the newest runs again. |
| `jobs discard <id>` | A waiting or failed job becomes cancelled |
| `jobs cancel <id>` | A waiting job is cancelled at once; a running one is told to stop within one heartbeat |
| `jobs pause <queue> [--reason r]` / `jobs resume <queue>` | Stop or restart claiming from a queue; running jobs finish |
| `jobs processes` | The registry; `STALE` marks a worker silent for over 60 s |
| `jobs prune --dry-run` | What retention would delete now (the leader deletes, every minute) |

Procedures are in the [runbook](RUNBOOK.md#background-jobs).

### Metrics

Scrape each worker at `:9300/metrics`.

| Metric | From | Meaning |
|---|---|---|
| `jobs_claimed_total{queue}` | every worker | Jobs claimed |
| `jobs_completed_total{kind,outcome}` | every worker | Finished attempts: `succeeded`, `retrying`, `failed`, `cancelled`, `released`, `lease_lost`, `fence_rejected` |
| `jobs_duration_seconds{kind}` | every worker | Handler time |
| `jobs_wait_seconds{kind}` | every worker | From due to claimed: queue latency as each job saw it |
| `jobs_running{queue}`, `jobs_slots{queue}` | every worker | Busy and total handler slots |
| `jobs_fence_rejected_total`, `jobs_lease_lost_total` | every worker | Outcome writes refused and handlers cancelled because the lease was lost (each lost lease once, counted where the heartbeat cancels the handler). Both exist at 0 from the start and should stay there. |
| `jobs_db_errors_total{op}` | every worker | Failed database calls of the loops: `claim`, `listen`, `register`, `heartbeat`, `finish` (an outcome write, retried until it lands), `reap`, `prune`, `recurring`, `retention`, `gauges` |
| `jobs_available{queue}`, `jobs_scheduled{queue}`, `jobs_running_all{queue}` | the leader | Queue depth across all workers |
| `jobs_oldest_available_seconds{queue}` | the leader | Age of the oldest job ready to run. The best single alarm. |
| `jobs_failed_recent{kind}` | the leader | Failed in the last 24 h |
| `jobs_processes_live` | the leader | Workers with a recent heartbeat |
| `jobs_leader` | every worker | 1 on the leader, 0 on the others (`JobsNoLeader` relies on the 0s) |

Only the leader exports the queue-wide gauges, so sum or max them across workers. The `jobs_completed_total` series of every registered kind and outcome exist at 0 from the start, so `increase()` sees the first failure.

### Logs

One line per event. The outcome events (`jobs.failed` to `jobs.succeeded`) carry `job_id`, `kind`,
`queue` and `attempt`; the enqueue events carry `job_id`, `kind` and `queue`. No event carries a
job's arguments.

| Event | Level (default `RUST_LOG=info` shows warn and info) | Extra fields |
|---|---|---|
| `jobs.failed` | warn | `cause`, `error` (cut to 2,000 characters; not set for `bad_args`, whose message can quote argument values) |
| `jobs.retry_scheduled` | info | `cause`, `error`, `delay_s` |
| `jobs.cancelled`, `jobs.released` | info | |
| `jobs.bad_args` | error | `job_id` and `kind` only; the decode message is in the job's `errors` |
| `jobs.lease_lost`, `jobs.lease_expired`, `jobs.fence_rejected`, `jobs.reaped` | warn / error | the job, or the reaper's cause and new state |
| `jobs.succeeded`, `jobs.enqueued`, `jobs.duplicate`, `jobs.claimed`, `jobs.finished` | debug | |
| `process.started`, `process.stopping`, `process.stopped`, `process.pruned`, `process.self_fenced`, `leader.acquired`, `leader.lost`, `recurring.enqueued` | info / warn | |

### Alerts

| Alert | Fires when | Runbook |
|---|---|---|
| `JobsQueueStalled` | The oldest ready job in a queue is over 5 minutes old, for 10 minutes | [A queue is not moving](RUNBOOK.md#jobs-a-queue-is-not-moving) |
| `JobsFailing` | Jobs of one kind failed in the last 15 minutes: a worker finished an attempt as failed, or the leader's `jobs_failed_recent` rose (a job failed by the reaper after `max_crashes`) | [Failed jobs](RUNBOOK.md#jobs-failed-jobs) |
| `JobsNoLeader` | No worker has been leader for 5 minutes (no reaping, no retention) | [A queue is not moving](RUNBOOK.md#jobs-a-queue-is-not-moving) |
| `JobsWorkerDown` | A `jobs-worker` scrape target has not answered for 2 minutes | [A queue is not moving](RUNBOOK.md#jobs-a-queue-is-not-moving) |
| `JobsLeaseLost` | A handler lost its lease or an outcome write was refused in the last 15 minutes; a job may have run twice | [A lease was lost](RUNBOOK.md#jobs-a-lease-was-lost) |

All five read the workers' metrics, so they need the `jobs-worker` scrape job in
`deploy/monitoring/prometheus.yml` (commented out there until a worker exists to scrape);
`JobsWorkerDown` and `JobsLeaseLost` stay silent without it. The metrics table above says which
series each uses.

### Health

`/healthz` answers 200 while the process runs. `/readyz` answers 503 when the worker fenced
itself off, has not completed a heartbeat yet, its last heartbeat is older than 60 s (3
heartbeats), or the database does not answer in 2 s. The embedded worker has no such port.

### Deploying

- Jobs are off until `JOBS_ENABLED=true` is set on `serve` and `worker`. Turning them off again
  leaves waiting jobs in the table, untouched, for when they are turned back on.
- The jobs schema has its own migrations (`crates/jobs/migrations/`, table `_jobs_migrations`).
  `serve` and `worker` both apply them at start, after the app's. They are expand-only, as the app's
  are ([decision 0003](decisions/0003-expand-and-contract-migrations.md)). Deploy `serve` first
  and start the worker after it: the worker also runs the app's migrations and the jobs ones, on
  its own pool.
- A new version of a job kind (`loan_email.v2`) is deployed to workers first; a worker never
  claims a kind it does not know. Remove the old version only after `jobs status` shows none waiting.
- Docker: the worker is the same image with `command: ["worker"]` and `stop_grace_period: 40s`.
  The load-test box has it under the compose profile `jobs`
  ([loadtest.web-pg.yml](../deploy/compose/local/loadtest.web-pg.yml)).
- Stopping: on SIGTERM a worker stops claiming, waits up to `JOBS_SHUTDOWN_TIMEOUT_S` for its jobs,
  cancels the rest and releases them (the attempt is given back, so a released job does not use
  up an attempt). With a stuck handler and a healthy database that is about 35 s; with the
  database dead up to about 50 s, which the 40 s grace period does not cover: the supervisor's
  SIGKILL then leaves the leases to expire and the leader retries the jobs. A second signal does
  not shorten the wait.
- `JOBS_EMBEDDED=true` cannot promise a clean exit within 40 s: the HTTP drain comes first and
  the worker's stop after it. If the embedded worker fences itself or fails, `serve` shuts down
  and exits non-zero so the supervisor restarts it.

## Adding a job

1. A file in `apps/web/src/jobs/` with a `Serialize + Deserialize` struct and `impl Job`: `KIND`
   with a version (`name.v1`), `QUEUE`, `perform`. Return `JobError::Retry` for what may pass later,
   `JobError::Permanent` for what never will.
2. Make it safe to run twice: give the receiver a stable id, or check before acting.
3. Register it in `jobs::registry()`, with what it needs via `.provide(Arc::new(..))`. Register it
   only when its settings exist, so its jobs wait instead of failing on a worker without them.
4. Enqueue it inside the transaction of the change that causes it (see `LoanEvents`).
5. If it needs a queue of its own, add it to the default `JOBS_QUEUES`.
6. Tests: the handler with `JobContext::detached`, the enqueue through the router (and that a
   refused request enqueues nothing).

## Testing

```bash
export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh)   # throwaway Postgres on 55432
eval "$(./scripts/test-mailpit.sh)"                      # throwaway Mailpit: SMTP 55025, API 58025
cargo test -p library-jobs                               # the queue
cargo test -p library-web --features postgres            # the app, including the jobs
cargo test -p library-jobs --test outage -- --ignored    # pauses its own Postgres for 20 s
cargo run --release -p library-jobs --example throughput -- "$TEST_POSTGRES_URL"   # use a scratch database
```

`make test` runs the whole workspace and then `library-web` with `--features postgres`, starting the
throwaway Postgres and Mailpit itself; the outage test and the throughput example are run by hand.
Throughput on the 2-core box: 16,700 no-op jobs/s with one worker and 64 slots (about 19,000 on 4
cores), with Postgres on the same machine.

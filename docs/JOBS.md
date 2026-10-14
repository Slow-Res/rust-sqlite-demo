# Background jobs

> **Draft.** Describes the design in
> [the spec](superpowers/specs/2026-10-14-background-jobs-design.md), built by
> [the plan](superpowers/plans/2026-10-14-background-jobs.md) on branch `feat/jobs`. Not
> implemented yet. Plan Task 20 checks every command, variable and number here against the code
> and removes this note.

Borrowing or returning a book sends the member an email and tells outside systems through signed
webhooks. Neither may slow the request or fail it when the mail server or a receiver is down, and
neither may happen for a loan change that rolled back. So the request only writes a row in the
`jobs` table, in the same transaction as the loan, and a separate worker process does the sending.

Postgres only. The SQLite deployment has no jobs. The queue is the generic crate
[`library-jobs`](../crates/jobs/README.md) (how to use it from axum, tokio or another framework is
in its README). The app's jobs live in `apps/web/src/jobs/`. Why we built the crate:
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
- **Leases**: a running job belongs to one worker until `locked_until`, 60 s ahead. Each worker
  renews the leases of all its jobs every 20 s, in one statement (the heartbeat).
- **The registry** (`job_processes`) has one row per worker, updated by the same heartbeat.
- **The leader** is whichever worker holds a Postgres advisory lock. It re-queues jobs whose lease
  ran out (every 5 s), deletes workers silent for 3 minutes (every 30 s), enqueues recurring jobs,
  deletes old jobs (succeeded after 24 h, failed after 14 days) and publishes the queue gauges.
  If its connection dies, another worker takes over within 10 s.

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

Both retry up to 10 attempts with backoff from 5 s doubling to 1 h, with full jitter: about 3 to 4
hours in all.

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
  so one bad job cannot take down every worker in turn.

## Configuration

Set on both `serve` and `worker` (they must agree on what is enqueued and what can be run).

| Variable | Default | Meaning |
|---|---|---|
| `JOBS_ENABLED` | `false` | Enqueue on borrow and return, migrate the jobs schema on start, allow `worker`. Needs a Postgres `DATABASE_URL`. |
| `JOBS_EMBEDDED` | `false` | `serve` also runs a worker |
| `JOBS_QUEUES` | `email:4,webhooks:8` | `name:slots[:max_running]`: handlers at once per queue in this process, and optionally across all processes |
| `JOBS_POLL_MS` | `1000` | How often an idle queue looks for work when no notification came |
| `JOBS_NOTIFY` | `true` | `NOTIFY` on enqueue and `LISTEN` in workers |
| `JOBS_DB_POOL` | `16` | The worker's own connection pool |
| `JOBS_SHUTDOWN_TIMEOUT_S` | `25` | On SIGTERM, how long running jobs may finish before they are cancelled and released |
| `JOBS_KEEP_SUCCEEDED_H` | `24` | Retention of succeeded jobs |
| `JOBS_KEEP_FAILED_H` | `336` | Retention of failed and cancelled jobs (14 days) |
| `JOBS_METRICS_ADDR` | `0.0.0.0:9300` | The worker's `/metrics`, `/healthz`, `/readyz`. Keep it off the public network. |
| `JOBS_SMTP_URL` | unset | `smtp://host:25`, `smtp://user:pass@host:587?tls=required` (STARTTLS) or `smtps://user:pass@host:465`. Unset: no emails are enqueued, and the worker does not run `loan_email.v1`. |
| `JOBS_SMTP_TIMEOUT_S` | `30` | Per connection and per SMTP reply |
| `JOBS_EMAIL_FROM` | `library@localhost` | Sender address |
| `JOBS_EMAIL_DOMAIN` | `library.local` | Right-hand side of the `Message-ID` |
| `JOBS_WEBHOOK_ENDPOINTS` | unset | Comma-separated `http(s)` URLs, at most 10 |
| `JOBS_WEBHOOK_SECRET_<n>` | – | Secret of the n-th endpoint (1-based): `whsec_` and the base64 of a 24 to 64 byte key |

Generate a webhook secret with `echo "whsec_$(openssl rand -base64 32)"`. Secrets and the SMTP URL
are never logged and never stored in job arguments.

## Operating

### Commands

All run with the worker's environment (`DATABASE_URL`), e.g.
`docker compose exec worker library-web jobs status`.

| Command | Does |
|---|---|
| `jobs status` | Per queue: available, scheduled, running, failed in 24 h, oldest wait in seconds, paused. The workers and their heartbeat age. A warning for kinds no live worker can run. |
| `jobs list [--state s] [--queue q] [--kind k] [--limit n]` | Newest first, with the last error |
| `jobs show <id>` | Everything: arguments, errors (up to 10, 2,000 characters each), progress, lease |
| `jobs retry <id>` | A failed or cancelled job runs again, attempts reset |
| `jobs retry --failed [--kind k] [--since 2h]` | The same, in bulk |
| `jobs discard <id>` | A waiting or failed job becomes cancelled |
| `jobs cancel <id>` | A waiting job is cancelled at once; a running one is told to stop within one heartbeat |
| `jobs pause <queue> [--reason r]` / `jobs resume <queue>` | Stop or restart claiming from a queue; running jobs finish |
| `jobs processes` | The registry; `STALE` marks a worker silent for over 60 s |
| `jobs prune --dry-run` | What retention would delete now |

Procedures are in the [runbook](RUNBOOK.md#background-jobs).

### Metrics

Scrape each worker at `:9300/metrics`.

| Metric | From | Meaning |
|---|---|---|
| `jobs_completed_total{kind,outcome}` | every worker | Finished attempts: `succeeded`, `retrying`, `failed`, `cancelled`, `released`, `lease_lost`, `fence_rejected` |
| `jobs_duration_seconds{kind}` | every worker | Handler time |
| `jobs_wait_seconds{kind}` | every worker | From due to claimed: queue latency as each job saw it |
| `jobs_running{queue}`, `jobs_slots{queue}` | every worker | Busy and total handler slots |
| `jobs_db_errors_total{op}` | every worker | Failed claims, heartbeats, reaps |
| `jobs_available{queue}`, `jobs_scheduled{queue}`, `jobs_running_all{queue}` | the leader | Queue depth across all workers |
| `jobs_oldest_available_seconds{queue}` | the leader | Age of the oldest job ready to run. The best single alarm. |
| `jobs_failed_recent{kind}` | the leader | Failed in the last 24 h |
| `jobs_processes_live`, `jobs_leader` | the leader | Workers with a recent heartbeat; 1 on the leader |

Only the leader exports the queue-wide gauges, so sum or max them across workers.

### Alerts

| Alert | Fires when | Runbook |
|---|---|---|
| `JobsQueueStalled` | The oldest ready job in a queue is over 5 minutes old, for 10 minutes | [A queue is not moving](RUNBOOK.md#jobs-a-queue-is-not-moving) |
| `JobsFailing` | Jobs of one kind failed in the last 15 minutes | [Failed jobs](RUNBOOK.md#jobs-failed-jobs) |
| `JobsNoLeader` | No worker has been leader for 5 minutes (no reaping, no retention) | [A queue is not moving](RUNBOOK.md#jobs-a-queue-is-not-moving) |

### Health

`/healthz` answers 200 while the process runs. `/readyz` answers 503 when the worker fenced
itself off, its last heartbeat is older than 60 s, or the database does not answer in 2 s.

### Deploying

- Jobs are off until `JOBS_ENABLED=true` is set on `serve` and `worker`. Turning them off again
  leaves waiting jobs in the table, untouched, for when they are turned back on.
- The jobs schema has its own migrations (`crates/jobs/migrations/`, table `_jobs_migrations`).
  `serve` and `worker` both apply them at start, after the app's. They are expand-only, as the app's
  are ([decision 0003](decisions/0003-expand-and-contract-migrations.md)).
- A new version of a job kind (`loan_email.v2`) is deployed to workers first; a worker never
  claims a kind it does not know. Remove the old version only after `jobs status` shows none waiting.
- Docker: the worker is the same image with `command: ["worker"]` and `stop_grace_period: 40s`.
  The load-test box has it under the compose profile `jobs`.

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

`make test` does the first four. Throughput on the 2-core box: measured in plan Task 14,
recorded here in Task 20.

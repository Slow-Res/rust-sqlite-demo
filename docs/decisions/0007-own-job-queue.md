# 7. Our own Postgres job queue

Status: proposed (2026-10-15). Becomes accepted when `feat/jobs` merges.

## Context

Borrowing and returning a book must notify the member by email and outside systems by webhook.
The notification must exist if and only if the loan change commits, and sending it must not slow
the request or fail it when the mail server or a receiver is down. More background work follows:
reports, imports, long-running jobs (sub-projects 2 to 5 of the
[spec](../superpowers/specs/2026-10-14-background-jobs-design.md)).

That needs a transactional outbox, which means a queue in the same Postgres database as the data,
written with the caller's transaction.

## Decision

Build `library-jobs` (`crates/jobs`): jobs are rows in Postgres, inserted with the caller's
connection. Workers claim with `FOR UPDATE SKIP LOCKED`. Each running job has a lease that one
heartbeat per process renews. Outcome writes are fenced by worker and attempt. A process registry
and an advisory-lock leader re-queue the jobs of dead workers. Workers run as a separate
`library-web worker` process. The crate has no web framework in its core, so the Rails app can
share the schema later and other Rust apps can use it.

## Alternatives

We read the source of each, not only the docs.

- **apalis-postgres 1.0.** Accepts the caller's transaction (`push_tasks(&mut *tx)`). But
  heartbeats are per worker, not per job, so a hung job is not detected; there is no backoff in the
  database, no progress, no cross-process cancel; a duplicate key fails the whole insert; and it was
  two days old.
- **fang.** No outbox (it uses its own pool), a claim that is not atomic under async, and no
  recovery after a crash.
- **BullMQ (official Rust client).** Redis only, so no outbox with our data. It has per-job locks
  and a stalled check, but a lost lock does not stop the running job, and it trusts the client
  clock.
- **graphile_worker (Rust).** Postgres and transactional enqueue, but not examined deeply enough
  to depend on.
- **Solid Queue (Rails).** Not usable from Rust, but its model shaped ours: the process registry
  with heartbeats, failing the claimed jobs of dead processes, recurring de-duplication on
  `(task, run_at)`, capped counts for the dashboard. It has no per-job lease, timeout or cancel; we
  add those.
- **A broker (RabbitMQ, SQS, NATS).** Another service to run and back up, and no transaction
  shared with our data: we would still need an outbox table plus a relay.

## Consequences

- About 3,000 lines of Rust that we own: the claim, lease, fencing and leader code is where bugs
  would lose or duplicate jobs, so it carries the most tests (concurrent claims, fencing, leader
  failover, a real SIGKILL, a database outage).
- Postgres only. The SQLite deployment has no background jobs.
- The queue shares the database with the app: job traffic costs app capacity. At about 2 to 4
  statements per job, that is small next to the request load, and sub-project 5 measures it.
- At-least-once delivery: handlers must be idempotent or give receivers a stable id.

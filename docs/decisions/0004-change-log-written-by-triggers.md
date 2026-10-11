# 4. Change log written by triggers

Status: accepted (2026-10-11)

## Context

The worker must learn about every committed change to books, members and loans,
without slowing a request and without ever missing one.

## Decision

SQLite triggers write a `change_log` row for every insert, update and delete
([migration](../../migrations/sqlite/20261011000000_change_log.sql#L9)). A trigger
runs in the same transaction as the write, so a change is logged if and only if it
is committed. No app code has to remember to log, and cascaded deletes are logged
too.

The log says which row changed, not what it now contains. The worker reads the
row's current values when it ships. This has three effects:

- Adding a column to a synced table needs no trigger change.
- A replayed batch can never put an old value back.
- Coalescing: 50 updates to one row become one row in the batch
  ([coalesce](../../apps/sync-worker/src/changes.rs#L144)).

One rule follows from this: never write a synced table with `INSERT OR REPLACE`,
because the delete trigger would not fire (see ARCHITECTURE, "Adding a synced
table").

## Consequences

- A request pays for one extra small insert inside a transaction it already had.
- The worker polls the indexed table, because SQLite cannot signal another process.
  An idle poll costs microseconds.
- Postgres keeps only the current state. For a history, the trigger would have to
  store the row's values in the log. That is future work.
- `users` and `sessions` have no triggers, so they never leave the node.

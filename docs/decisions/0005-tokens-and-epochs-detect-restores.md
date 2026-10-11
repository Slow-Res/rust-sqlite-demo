# 5. Tokens and epochs detect restores

Status: accepted (2026-10-11)

## Context

A node can be restored from a backup that is older than what Postgres has already
received. The restored change log then has lower numbers, and new writes reuse
`seq` numbers that Postgres's checkpoint already counts as shipped. Trusting the
checkpoint would skip those writes for ever.

## Decision

Every change_log row gets a random 8-byte `token`. The checkpoint in Postgres stores
the `seq` and `token` of the last row shipped. At the start of every cycle the worker
checks that the log still has a row with that `seq` and the same token
([checkpoint_is_valid](../../apps/sync-worker/src/changes.rs#L167)). If not, the node
was restored, and the worker does a full copy
([replace_all](../../apps/sync-worker/src/mirror.rs#L102)):

- Rows Postgres had for the node go to `sync_archive`, tagged with the old epoch.
- The node's mirror rows are replaced by the current snapshot.
- The checkpoint moves to `epoch + 1`.

All of it happens in one Postgres transaction. The worker never prunes the row at
`last_seq` and `AUTOINCREMENT` never reuses a number, so in normal running the check
always passes. A checkpoint with `last_seq = 0` is always valid, so a change log
emptied by hand cannot cause a resync loop.

## Consequences

- A restore is noticed without any manual step, whichever side is older.
- Nothing is thrown away: rows only Postgres had stay in `sync_archive`, and a
  person decides what to do (RUNBOOK, "After a resync").
- Each resync raises `sync_resyncs_total` and the `SyncResync` alert.
- The full copy reads every row in one read transaction. That is fine at this size;
  large databases would need a chunked copy.

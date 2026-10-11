# 3. Expand-and-contract migrations

Status: accepted (2026-10-11)

## Context

A node has no spare database. We cannot restore a backup every time a release goes
wrong, because a restore loses recent writes. A rollback must therefore be a code
change only. Also, during a rolling deploy, an old worker on one node may still run
against a newer Postgres schema, and the other way round.

## Decision

Schema changes happen in two or more releases:

1. **Expand.** Add the new column or table. Keep the old one. The previous app
   version must still work on the new schema.
2. **Migrate.** Deploy code that uses the new shape and move the data.
3. **Contract.** In a later release, remove the old column.

Migrations are forward-only and are never edited after they are merged (SQLx
checksums catch edits). The same rule applies to Postgres migrations.

Only `library-web` runs SQLite migrations. Only `sync-worker` runs Postgres
migrations. The worker waits until `library-web` has applied every SQLite migration
it knows before it ships anything
([schema.rs](../../apps/sync-worker/src/schema.rs#L12)).

## Consequences

- Rolling back is deploying the old image. No database restore is needed.
- A rename takes three releases instead of one.
- A change to books, members or loans needs a matching Postgres migration in the
  same change. A contract test fails if the two sides disagree.

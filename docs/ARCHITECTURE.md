# Architecture

How the parts fit together, what happens on a write, and what is and is not
guaranteed. The full design, with the reasons behind each choice, is in the
[spec](superpowers/specs/2026-10-10-sqlite-production-design.md). Big decisions have
short records in [decisions/](decisions/).

## The picture

```
                 users of branch A          users of branch B          users of branch C
                        |                          |                          |
+------------ VM: node1 -------------+  +---- VM: node2 -----+  +---- VM: node3 -----+
|  web (Axum + React)                 |  |  same 4 containers  |  |  same 4 containers  |
|     | reads and writes              |  +----------+----------+  +----------+----------+
|     v                               |             |                        |
|  /data/library.db  (SQLite, WAL)    |             |                        |
|     |                    |          |             |                        |
|  litestream          sync-worker    |             |                        |
+-----+--------------------+----------+             |                        |
      | every change,      | batches of changed      |                        |
      | about 1 s behind   | rows, about 1 s behind  |                        |
      v                    v                         v                        v
  S3 bucket            Postgres  <-------------------+------------------------+
  node1/ node2/ node3/   books/members/loans, each row tagged with node_id
  (backup used to        (second copy, readable with SQL: reports across
   restore a node)        branches, last-resort recovery)
```

The two backups do different jobs.

| | Litestream to S3 | sync-worker to Postgres |
|---|---|---|
| What it copies | The database file, page by page | Rows of books, members and loans |
| What it is for | Bringing a node back after its disk is lost | Reading every branch in one place; last-resort recovery |
| Does the app read it? | No | No |
| Lag | About 1 s | About 1 s (200 ms to 2 s polling) |

SQLite is the primary database. The app reads and writes only its local file, so a
request never touches the network. Nothing flows from Postgres back to SQLite.

## One write, step by step

A new book, from the browser to Postgres.

| # | What happens | Where |
|---|---|---|
| 1 | The browser sends `POST /api/books`. | [routes.rs](../apps/web/src/books/routes.rs#L37) |
| 2 | The app runs one `INSERT`. (Updates and deletes run in a `BEGIN IMMEDIATE` transaction instead, so a check and its write cannot be separated by another writer.) | [sqlite.rs](../apps/web/src/books/sqlite.rs#L176), [update](../apps/web/src/books/sqlite.rs#L205) |
| 3 | A trigger adds a `change_log` row in the same transaction. | [migration](../migrations/sqlite/20261011000000_change_log.sql#L18) |
| 4 | The transaction commits. The response is sent. Nothing so far waited for the network. | |
| 5 | Litestream uploads the new WAL pages to S3, within about 1 s. | [litestream.yml](../deploy/docker/litestream/litestream.yml) |
| 6 | The worker reads the next batch of up to `BATCH_SIZE` log rows and the rows' current values. It polls every 200 ms while busy and backs off to 2 s while idle. | [read_batch](../apps/sync-worker/src/changes.rs#L99), [next_poll](../apps/sync-worker/src/run.rs#L53) |
| 7 | One Postgres transaction applies the batch (one insert-or-update and one delete statement per table) and moves the checkpoint. | [apply_batch](../apps/sync-worker/src/mirror.rs#L102) |
| 8 | The worker deletes log rows below the new checkpoint. The row at the checkpoint stays, because the next cycle checks it. | [prune](../apps/sync-worker/src/changes.rs#L303) |

Why nothing is lost or repeated:

- Postgres data and the checkpoint commit together, so they cannot disagree.
- If the worker dies after the Postgres commit and before the prune, the next cycle
  starts from the saved checkpoint and the prune catches up. If the prune itself
  fails after a commit, the outcome stands and the failure is counted in
  `sync_errors_total` (the shipped rows stay in the log until a later prune).
- Upserts and deletes by `(node_id, id)` give the same result when repeated. Current
  values are shipped, so a replay cannot put an old value back.
- The worker never skips a change. A batch it cannot apply fails and is retried.

## Guarantees

| Situation | What can be lost |
|---|---|
| Lost disk | About 1 s of writes (Litestream's interval). |
| Postgres | It never loses what the node still has. After a node is restored from an older backup, Postgres follows the node and keeps the rest in `sync_archive`. |
| App during an outage | It keeps working when Postgres or S3 is down. Nothing in a request waits for either. |

One gap exists. When a batch is full (the node had more than `BATCH_SIZE`
changes waiting), the worker ships the rows' *current* values, read in one
transaction, and those can be newer than the batch's last change
([read_batch](../apps/sync-worker/src/changes.rs#L99) reads the rows after the
changes). The checkpoint still points at the batch's last `seq`. If the node is then
restored to a point between the two, that checkpoint row is still there with the same
token, so the check passes and there is no resync. Postgres keeps the newer values
the node lost, until those rows change again. The gap is bounded by the Litestream
window (what the backup had not yet uploaded), and only happens with a backlog larger
than `BATCH_SIZE`. A partial batch has no such gap, because it covers every change
up to the moment it was read.

What Postgres is: a readable copy of each branch's books, members and loans, for
reports and last-resort recovery.

What Postgres is not: a source to restore a node from (recovering a node from
Postgres alone is a manual job), and a copy of everything (it has no user accounts
or sessions, so password hashes never leave the node).

## SQLite rules

Each rule is enforced by code, by config or by a test, except S7 and S16, which are
notes.

| # | Rule | Where it is enforced |
|---|---|---|
| S1 | Local disk only, one host. | A named local volume in [node.yml](../deploy/compose/node.yml#L86); RUNBOOK warns. |
| S2 | WAL journal mode, so readers never block the writer. | [sqlite-setup](../crates/sqlite/src/lib.rs#L35), on every connection. |
| S3 | `synchronous = NORMAL`: safe with WAL; a power cut can lose the last commits but never corrupts the file. | [sqlite-setup](../crates/sqlite/src/lib.rs#L36) |
| S4 | `busy_timeout = 5000`: wait up to 5 s for the write lock. | [sqlite-setup](../crates/sqlite/src/lib.rs#L37) |
| S5 | `foreign_keys = ON` on every connection (SQLite has it off by default). | [sqlite-setup](../crates/sqlite/src/lib.rs#L34) |
| S6 | Write transactions that read then write start with `BEGIN IMMEDIATE`. | [books](../apps/web/src/books/sqlite.rs#L205), [loans](../apps/web/src/loans/sqlite.rs#L158), [members](../apps/web/src/members/sqlite.rs#L170) |
| S7 | Write transactions are short and never wait on the network. | A code review rule. |
| S8 | Every pragma is applied to every connection, by the pool's connect options. | [test](../crates/sqlite/src/lib.rs#L122) opens two connections and checks both. |
| S9 | Only `library-web` runs SQLite migrations. They are forward-only and never edited after merge. | [migrate](../apps/web/src/db.rs#L20); the worker only [embeds](../apps/sync-worker/src/lib.rs#L21) them to check. |
| S10 | Schema changes are expand then contract. | [Decision 0003](decisions/0003-expand-and-contract-migrations.md) |
| S11 | A change to a synced table needs a matching Postgres migration in the same change. | [contract test](../apps/sync-worker/tests/contract.rs#L28) |
| S12 | Never copy a live database with `cp`; use `litestream restore` or `VACUUM INTO`. Never delete `library.db` without its `-wal` and `-shm` files. | RUNBOOK; the [restore gate](../deploy/docker/litestream/restore.sh#L26) refuses leftover files. |
| S13 | One Litestream per database, and a unique `NODE_ID` per node. | The gate [validates](../deploy/docker/litestream/restore.sh#L13) `NODE_ID`; the worker's [lock](../apps/sync-worker/src/mirror.rs#L78) detects a duplicate. |
| S14 | Backups are tested by restoring them. | [restore-drill.sh](../scripts/restore-drill.sh) and the [e2e test](../scripts/e2e.sh#L125) |
| S15 | The database is checked after every restore and on demand. | The gate's [`-integrity-check quick`](../deploy/docker/litestream/restore.sh#L37); [`library-web check-db`](../apps/web/src/commands.rs#L121). |
| S16 | Checkpointing stays at SQLite's default (every 1000 pages). | A note, below. |

On S16: Litestream 0.5 copes with checkpoints made by the app. At very high write
rates (tens of thousands of transactions a second), set `wal_autocheckpoint = 0` and
let Litestream checkpoint. Nothing here needs that.

`SQLITE_CHECKPOINT_INTERVAL_MS` (default 0, off) starts a background task in
`library-web` that runs `PRAGMA wal_checkpoint(TRUNCATE)` that often
([checkpoint.rs](../apps/web/src/checkpoint.rs)). SQLite's own checkpoints are PASSIVE,
and under constant readers they never let the WAL reset: in the web-only load test it
reached 2.5 GB for a 43 MB database and stalled requests. It is off wherever Litestream
runs, because Litestream reads the WAL to replicate it and does the checkpoints itself;
the load-test web-only mode (no Litestream) sets it to 1000. A busy checkpoint logs a
warning with the pages left in the WAL.

S7 in practice: do not call another service, wait on a channel, or read a slow file
inside a write transaction. The write lock is held until the transaction ends, and
every other writer waits for it.

## How a restore is detected

The danger: a node is restored from a backup older than what Postgres has already
received. The restored log reuses `seq` numbers that Postgres's checkpoint already
counts as shipped, and new writes would be skipped for ever.

The defence: every change_log row has a random 8-byte `token`. The checkpoint in
Postgres stores the `seq` and `token` of the last row shipped. At the start of every
cycle the worker looks for that `seq` in the log and compares the token
([checkpoint_is_valid](../apps/sync-worker/src/changes.rs#L167)). A checkpoint with
`last_seq = 0` is always valid.

| Situation | Log row at the checkpoint's `seq` | Result |
|---|---|---|
| Normal running | Present, same token | Carry on |
| Restored from a backup newer than the checkpoint (the usual case) | Present, same token | Carry on |
| Restored from a backup older than the checkpoint | Missing | Full copy |
| ...and the app has already reused those numbers | Present, different token | Full copy |
| Postgres itself restored to an older point | Missing | Full copy |
| New Postgres with no checkpoint | n/a | Full copy (first sync) |

A full copy is also called a resync ([full_copy](../apps/sync-worker/src/sync.rs#L120)).
In one Postgres transaction it:

1. copies the node's current mirror rows into `sync_archive`, tagged with the old
   epoch;
2. deletes them from the mirror;
3. inserts the node's current rows, in chunks of 1000
   ([COPY_CHUNK](../apps/sync-worker/src/mirror.rs#L18));
4. moves the checkpoint to `epoch + 1`, the newest log `seq` and its token.

Anyone reading Postgres sees the old copy or the new one, never a mix. Each resync is
logged as a warning, counted in `sync_resyncs_total` and raises the `SyncResync`
alert. After a restore it means the node lost some writes; the rows are in
`sync_archive` (RUNBOOK, "After a resync").

## Module map

### apps/sync-worker/src

| Module | Job |
|---|---|
| [config.rs](../apps/sync-worker/src/config.rs) | Settings from environment variables; validates `NODE_ID`. |
| [tables.rs](../apps/sync-worker/src/tables.rs) | The list of synced tables and the plain row type. |
| [changes.rs](../apps/sync-worker/src/changes.rs) | SQLite side: read batches and snapshots, check the checkpoint, prune. |
| [mirror.rs](../apps/sync-worker/src/mirror.rs) | Postgres side: connection, node lock, apply a batch, full copy. |
| [sync.rs](../apps/sync-worker/src/sync.rs) | One sync cycle: check, ship a batch or copy everything. |
| [schema.rs](../apps/sync-worker/src/schema.rs) | Checks that `library-web` has applied every SQLite migration. |
| [run.rs](../apps/sync-worker/src/run.rs) | The main loop: waits, sessions, retries with growing waits. |
| [metrics.rs](../apps/sync-worker/src/metrics.rs) | Prometheus metrics. |
| [http.rs](../apps/sync-worker/src/http.rs) | `/health` and `/metrics` on port 9100. |
| [status.rs](../apps/sync-worker/src/status.rs) | `sync-worker status`: epoch, last shipped `seq`, changes waiting. |
| [cli.rs](../apps/sync-worker/src/cli.rs) | Command-line arguments (run, or `status`). |
| [telemetry.rs](../apps/sync-worker/src/telemetry.rs) | Logging setup. |

### apps/web/src

| Path | Job |
|---|---|
| `books/`, `members/`, `loans/`, `auth/` | One folder per feature, each with its routes, service, repository trait and SQLite implementation. |
| [app.rs](../apps/web/src/app.rs), [server.rs](../apps/web/src/server.rs) | The router, and serving it until shutdown. |
| [db.rs](../apps/web/src/db.rs) | The connection pool and the SQLite migrations. |
| [config.rs](../apps/web/src/config.rs), [state.rs](../apps/web/src/state.rs) | Settings, and the state handed to handlers. |
| [health.rs](../apps/web/src/health.rs) | `GET /api/health`, with a `SELECT 1` on the database. |
| [commands.rs](../apps/web/src/commands.rs), [cli.rs](../apps/web/src/cli.rs), [seed.rs](../apps/web/src/seed.rs) | `seed`, `create-user` and `check-db`. |
| `error.rs`, `extract.rs`, `pagination.rs`, `clock.rs`, `telemetry.rs` | Shared helpers: errors, request extractors, paging, a testable clock, logging. |

Shared crates: [crates/sqlite](../crates/sqlite/src/lib.rs) (package `sqlite-setup`)
is the one place SQLite is opened and configured. [crates/shared](../crates/shared/src/lib.rs)
holds the API types and `is_valid_node_id`.

### Background jobs (Postgres only)

> **Draft:** being built on `feat/jobs`; see [JOBS.md](JOBS.md). Links are added when the files exist.

`crates/jobs` (package `library-jobs`) is a generic Postgres job queue with no web framework in
its core; its [README](../crates/jobs/README.md) covers using it from axum, tokio and other
frameworks.

| Module | Job |
|---|---|
| `lib.rs` | Module list, re-exports, `migrate()` (its own `_jobs_migrations` table). |
| `job.rs`, `context.rs` | The `Job` trait, `JobOptions`, `JobError`; the `JobContext` a handler gets, with `require::<T>()`. |
| `enqueue.rs` | `Enqueuer`: insert a job on the caller's connection, de-duplicate on its unique key, `NOTIFY` on commit. |
| `config.rs`, `backoff.rs`, `rate_limit.rs` | Queue and worker settings, retry delays, per-process rate limits. |
| `store/` | The SQL: claim, heartbeat and registry, fenced outcome writes, reaping and retention, counts. |
| `registry.rs`, `executor.rs` | Kind to handler; running one job to a recorded outcome (deadline, panic, cancel). |
| `running.rs`, `heartbeat.rs` | The jobs this process runs, and the one task that renews their leases. |
| `worker.rs` | `Worker`: a claim loop per queue, LISTEN wake-ups, graceful shutdown, self-fencing. |
| `maintenance.rs` | The advisory-lock leader: reaper, pruner, recurring jobs, retention, gauges. |
| `metrics.rs`, `ready.rs`, `http.rs` | Prometheus metrics, readiness, and the optional axum router for `/metrics`, `/healthz`, `/readyz`. |
| `ops.rs` | Queries behind `library-web jobs …`. |

`apps/web/src/jobs/` holds the app's side:

| Module | Job |
|---|---|
| `mod.rs` | The registry of the app's kinds and the worker settings from `JOBS_*`. |
| `events.rs` | `LoanEvents`: the jobs borrow and return enqueue, inside their transaction. |
| `loan_email.rs`, `webhook.rs` | `loan_email.v1` (SMTP) and `webhook.v1` (signed HTTP). |
| `worker.rs`, `ops_cli.rs` | `library-web worker`, embedded mode, and `library-web jobs …`. |

Settings are in `apps/web/src/jobs_config.rs`, compiled into every build; everything else is
behind the `postgres` feature.

## Book search

Book search matches word prefixes: `q` is split into words (runs of letters and
digits; whitespace and punctuation separate them), and a book matches when every word
is a prefix of some word of its title, author or ISBN, ignoring case. "anc" finds
"Ancient", "fancy" does not. `q` with spaces and hyphens removed also matches the
start of the ISBN, so "978-0-261" finds "9780261...". Ordering and `total` are
unchanged. A `q` with no word at all (only punctuation) is matched as plain text,
a scan.

SQLite uses an FTS5 index, `books_fts`
([migration](../migrations/sqlite/20261014000000_prefix_search.sql)): the `unicode61`
tokenizer with `remove_diacritics 0` (accents stay significant) and `prefix = '2 3'`.
It is an external-content table over `books`; three triggers keep it in sync, with no
`WHEN` condition, so every write to `books` updates it. The match expression is every
word as a quoted prefix (`"anc"*`), ANDed; quoting keeps query syntax out of the
query. Postgres uses a GIN index on `book_search_vector(title, author, isbn)`
([migration](../migrations/web-postgres/20261014000000_prefix_search.sql)) and
`to_tsquery('simple', ...)` with `'word':*` terms; the ISBN form is `isbn LIKE 'digits%'`.
The same `REPLACE` warning as below applies: use `INSERT ... ON CONFLICT DO UPDATE`, or
the old row stays in the index. Member search keeps `LIKE`: the table is small.

## Adding a synced table

Three steps. The [contract test](../apps/sync-worker/tests/contract.rs#L28) fails and
names the table if any of them is missing or disagrees.

1. A SQLite migration with the table and its three change-log triggers
   (`<table>_logged_insert`, `_update`, `_delete`), like the
   [existing ones](../migrations/sqlite/20261011000000_change_log.sql#L18).
2. A Postgres migration with the mirror table: the same columns plus `node_id` and
   `synced_at`, with primary key `(node_id, id)` and no other constraints.
3. One entry in the worker's [TABLES](../apps/sync-worker/src/tables.rs#L63) list.

Never write a synced table with `INSERT OR REPLACE` or `REPLACE`. When REPLACE
deletes a conflicting row, SQLite fires the DELETE trigger only if
`recursive_triggers` is on, and it is off here. The change_log would miss the delete,
and the row would stay in Postgres for ever. `INSERT ... ON CONFLICT DO UPDATE` is
fine, because it fires the UPDATE trigger.

## Adding a column to a synced table

The worker reads a changed row's current values by id, with the column list from
[TABLES](../apps/sync-worker/src/tables.rs#L63)
([read_rows](../apps/sync-worker/src/changes.rs#L211)). Rows are not stored in the
change log, so the column list is the only thing that decides what is shipped.
Four things ship in one release:

1. The SQLite migration adds the column with a `DEFAULT`.
2. The Postgres migration adds the same column with the same `DEFAULT`, so rows
   Postgres already has match what SQLite holds for them.
3. `tables.rs` lists the column.
4. The web code that uses it (and, as always, expand before contract: see
   [decision 0003](decisions/0003-expand-and-contract-migrations.md)).

If `tables.rs` names a column that SQLite does not have yet, the `SELECT` fails, the
cycle fails and is retried, and the checkpoint does not move: nothing is skipped. The
new worker also waits until `library-web` has applied every SQLite migration it knows
([schema.rs](../apps/sync-worker/src/schema.rs#L12)), so this only shows up if the
schema really is missing.

On deploy, stop `sync-worker` **before** the new web image migrates SQLite (RUNBOOK,
"Deploy a new version"). Otherwise the old worker keeps running against the new
schema: it ships and prunes changes with its old column list, so the rows reach
Postgres without the new column, and the log rows that would say they changed are
gone. After the new worker starts it ships current row values, the new column
included, for every row that changes from then on. Rows that never change keep the
Postgres `DEFAULT`, which is why the two defaults must match.

Future work: a manual resync command, to re-copy a node on demand after a change like
this.

## List paging

`GET /api/books`, `/api/members` and `/api/loans` return a `Page`: `items`, `page`,
`per_page`, `total` and `next_cursor`. They page in two ways:

- **`page=N`** uses `OFFSET`. A deep page walks and discards every row before it, which on
  Postgres means a heap visit per skipped index entry (a random deep page of 50,000 books
  ran at 257 req/s against 3,900 for page 1).
- **`after=<cursor>`** is keyset paging: the page starts strictly after the item the
  cursor names, in the list's own order (books `title, id`; members `name, id`; loans
  `due_at, id`, or `returned_at DESC, id DESC` for `status=returned`), so every page costs
  the same as page 1. `page` is ignored when `after` is given. Filters still apply.

`next_cursor` is the cursor of the last item when another page may follow, else `null`. It
is set for `page=N` requests too, so a client can start with page 1 and continue with
`after`. The repository fetches `per_page + 1` rows to know whether there is a next page.
`total` is unchanged (the cached count).

A cursor is opaque: URL-safe base64 (no padding) of the JSON `[sort key, id]`. It is decoded
into typed values and bound as parameters, never put into SQL, so it cannot inject. A
cursor that is not valid base64, not that JSON, or has the wrong key type for the list is a
400 `bad_request`. The SQL is a row-value comparison, `WHERE (b.title, b.id) > (?, ?)`
(`<` for returned loans), which both SQLite and Postgres serve as an index range scan
(`idx_books_title`, `idx_books_title_id`, ...) with no sort and no `OFFSET`.

## Future work

- A manual resync command, to re-copy one node on demand.
- A chunked full copy, for databases too large to read in one transaction.
- A history table in Postgres. It needs the trigger to store the row's values in the
  log.
- Zero-downtime deploys. A redeploy now means about 1-2 s of downtime.
- Separate Postgres roles for migrations and for the runtime.
- Host metrics with disk alerts.
- Alertmanager, to send the alerts somewhere. Today they are only visible in
  Prometheus.

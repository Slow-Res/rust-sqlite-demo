# SQLite in Production: Design

Date: 2026-10-10. Status: draft for review.

## 1. Goal

Show, with a complete working example, **how to run SQLite as the production database
safely and maintainably**. Several servers (called *nodes*) each run their own copy of a
web app on their own SQLite file. Every node is self-sufficient. Two independent backups
protect each node's data.

### What you asked for

- Many VMs, each running the web framework, SQLite and a sync worker. Each VM is
  self-sustained.
- Users are sticky: the same user always lands on the same VM.
- SQLite is the production database and must survive redeploys. Production must work
  even if Postgres does not exist.
- Postgres is only a backup. Data flows one way, SQLite → Postgres, written through
  "smartly and efficiently". Nothing flows from Postgres back to SQLite.
- Litestream streams every SQLite change to an S3 bucket.
- Containers are only for deployment. Day-to-day coding happens locally.
- A new project with a clear folder structure: every folder and module has one job.
- What Postgres will eventually be used for is not decided yet, so the layout should
  leave room.

### What success looks like

1. `make up` starts Postgres, an S3 stand-in and three nodes on one laptop.
   Each node is the same `node.yml` you would copy to a real VM.
2. Writes on any node show up in Postgres within about a second, tagged with the
   node's id.
3. Deleting a node's disk and starting it again brings all of its data back from S3,
   with no manual steps. The sync to Postgres carries on without duplicating or losing
   anything.
4. Every failure in §11 has a test or a runbook entry, and most have both.
5. A reader can open any folder and know its job from §4 and its README line.

### Assumptions I made (please confirm or correct)

| # | Assumption | Why |
|---|---|---|
| A1 | The project lives at `~/rust/sqlite-production`, a new git repo. | Separate from the library app, as you asked. |
| A2 | The example app is the library app (Axum backend + React frontend), copied in. Leptos is left out. | A real app with real tables makes the sync meaningful. React is the version we ship in Docker. |
| A3 | **Each node is one library branch**: its own books, members, loans and staff accounts. | Sticky users only work if a node owns its data. Shared data across nodes would need two-way sync, which you ruled out. |
| A4 | Books, members and loans go to Postgres. User accounts and sessions do not. | Password hashes and session tokens should not be copied to a central database. Litestream already backs them up. |
| A5 | Postgres holds the **current state** of each row, not a history of changes. | It is a backup copy. A history log can be added later (§6.9). |
| A6 | "Smartly and efficiently" means: the request never waits for Postgres. Changes are sent in batches, and a row that changed 50 times is sent once. | This is the usual meaning of *write-behind*. |
| A7 | If a node is restored from an older backup, Postgres follows the node (the node is the truth). Rows that only Postgres had are kept in an archive table, not thrown away. | It respects "SQLite is the source of truth" without silently losing data. |
| A8 | A Prometheus server with alert rules is included. Grafana is not. | Lag alerts are the safety net of this design. Dashboards are extra. |
| A9 | A brand-new node must be started once with `ALLOW_FRESH_DB=true`. | This stops a misconfigured node from starting empty and overwriting a good backup (§7.2). |

## 2. The big picture

```
                 users of branch A          users of branch B          users of branch C
                        │                          │                          │
┌───────────── VM: node1 ─────────────┐  ┌──── VM: node2 ────┐  ┌──── VM: node3 ────┐
│  web (Axum + React)                 │  │  same 4 containers │  │  same 4 containers │
│     │ reads and writes              │  └─────────┬──────────┘  └─────────┬──────────┘
│     ▼                               │            │                       │
│  /data/library.db  (SQLite, WAL)    │            │                       │
│     │                    │          │            │                       │
│  litestream          sync-worker    │            │                       │
└─────┼────────────────────┼──────────┘            │                       │
      │ every change,      │ batches of changed     │                       │
      │ about 1 s behind   │ rows, about 1 s behind │                       │
      ▼                    ▼                        ▼                       ▼
  S3 bucket            Postgres  ◄──────────────────┴───────────────────────┘
  node1/ node2/ node3/   books/members/loans, each row tagged with node_id
  (backup used to        (second copy, readable with SQL: reports across
   restore a node)        branches, last-resort recovery)
```

- **SQLite is the primary database.** The app reads and writes only its local file.
  Requests never touch the network.
- **Litestream → S3 is the physical backup.** It copies the database page by page. It is
  what brings a node back after its disk is lost. It can lose about the last second of
  writes (§11).
- **sync-worker → Postgres is the logical backup.** It copies rows, not pages. It gives a
  readable copy of every branch in one place. The app never reads from it.

## 3. Deployment layout: one node

Each VM runs four containers. They share one Docker volume on the VM's local disk, which
holds `library.db`.

| Container | Image | Job | Runs |
|---|---|---|---|
| `restore` | ours (Litestream inside) | If `library.db` is missing, restore it from S3. If it exists, do nothing. Refuses to start empty unless told to (§7.2). | Once at start, then exits |
| `litestream` | ours (Litestream inside) | Streams every change to `s3://<bucket>/<node-id>/`. Exposes metrics on :9090. | Always |
| `web` | ours | The Axum app with the React build. Owns the SQLite schema: runs SQLite migrations at start. | Always |
| `sync-worker` | ours | Reads the change log, ships batches to Postgres. Owns the Postgres schema. Only container with Postgres credentials. Metrics on :9100. | Always |

**Start order:** `restore` finishes, then `litestream` starts, then `web` starts, then
`sync-worker` starts once `web` is healthy. Compose stops containers in reverse order,
so on shutdown `web` stops first and `litestream` copies the last writes before it
stops.

**Rules for the volume:**
- It must be on the VM's local disk. Never NFS, EFS or any network filesystem: SQLite's
  locking is unreliable there and the file can be corrupted.
- All four containers run as the same non-root user (uid 10001), so every process can
  open the database and its `-wal` and `-shm` files.
- Only one host may use it.

**Why four containers and not one:** each one has a single job, its own logs, its own
health check and its own restart. If Postgres is down, only `sync-worker` is unhappy; the
app keeps serving. The web binary contains no Postgres code at all. The cost is a little
more compose configuration. Doing it all in one container (Litestream's `-exec` mode
launching the app and worker) was considered and rejected (decision record 0002).

## 4. Repository structure

Each folder has one job. Changes from what you saw in chat:
- One `Dockerfile` with two build targets instead of two Dockerfiles. Both Rust programs
  then share one compile step.
- `deploy/compose/local/` instead of `local.yml`. The local cluster now runs the real
  `node.yml` three times.
- `deploy/monitoring/` for Prometheus.

```
sqlite-production/
├── apps/                          programs you run
│   ├── web/                       Rust: the Axum server (library backend). Package `library-web`.
│   ├── sync-worker/               Rust: change log → Postgres. Package `sync-worker`.
│   └── frontend/                  React app; its build is served by `web`.
├── crates/                        Rust libraries the programs share
│   ├── shared/                    API types used by web and frontend; generates the
│   │                              TypeScript types. Package `library-shared`.
│   └── sqlite/                    The one place SQLite is opened and configured (§5).
│                                  Used by web and sync-worker. Package `sqlite-setup`.
├── migrations/
│   ├── sqlite/                    App tables, change log and triggers. Run by web.
│   └── postgres/                  Mirror tables, checkpoints, archive. Run by sync-worker.
├── deploy/
│   ├── docker/
│   │   ├── Dockerfile             targets `web` and `sync-worker`
│   │   ├── litestream.Dockerfile  official Litestream image + our config and restore script
│   │   └── litestream/            litestream.yml, restore.sh
│   ├── compose/
│   │   ├── node.yml               one VM: restore + litestream + web + sync-worker
│   │   ├── node.env.example       every setting a node needs, documented
│   │   ├── central.yml            Postgres + Garage (S3) + Prometheus
│   │   └── local/                 the 3-node laptop cluster: node1.env, node2.env,
│   │                              node3.env, node.override.yml (joins the shared network)
│   ├── garage/                    garage.toml and init.sh (bucket + fixed dev key)
│   └── monitoring/                prometheus.yml and alerts.yml
├── scripts/                       e2e.sh, restore-drill.sh, test-postgres.sh
├── docs/
│   ├── ARCHITECTURE.md            how the parts fit, data flow, guarantees
│   ├── RUNBOOK.md                 step-by-step procedures (§13)
│   ├── decisions/                 one short record per big decision (§13)
│   └── superpowers/specs/         this document
├── Makefile                       every command you need (§10)
├── .github/workflows/ci.yml
└── README.md                      quick start and a one-line job for every folder
```

**Boundaries:**
- `migrations/` is at the top because the change log and its triggers are a contract
  between `web` (which writes them) and `sync-worker` (which reads them). Neither owns it
  alone. `web` is still the only program that *runs* the SQLite migrations.
- `crates/sqlite` exists so that the web app and the worker cannot drift apart in how
  they open the same file.
- Only `apps/sync-worker` depends on a Postgres driver.

## 5. SQLite production rules

These are the rules the example demonstrates. Each one is enforced by code, by config or
by a test, unless it says otherwise.

| # | Rule | Where it is enforced |
|---|---|---|
| S1 | Local disk only, one host. | `node.yml` uses a named local volume; RUNBOOK warns. |
| S2 | WAL journal mode: readers never block the writer. | `crates/sqlite`, on every connection. |
| S3 | `synchronous = NORMAL`. It is safe with WAL: a power cut can lose the last commits but never corrupts the file. This is also what Litestream recommends. | `crates/sqlite` |
| S4 | `busy_timeout = 5000`: wait up to 5 s for the write lock instead of failing at once. | `crates/sqlite` |
| S5 | `foreign_keys = ON` on every connection. SQLite has it off by default. | `crates/sqlite` |
| S6 | Write transactions start with `BEGIN IMMEDIATE`, which takes the write lock up front. A plain `BEGIN` that reads and then writes can fail with `SQLITE_BUSY` halfway. | Kept from the library app; `crates/sqlite` documents it. |
| S7 | Write transactions are short and never wait on the network. | Code review rule, written in ARCHITECTURE.md. |
| S8 | Every pragma is applied to every connection, by the pool's connect options. Running pragmas once on one connection is a common bug. | `crates/sqlite` unit test opens two connections and checks both. |
| S9 | Only one program runs SQLite migrations (`web`). Migrations are forward-only and never edited after they are merged; SQLx checksums catch edits. | `web` startup; SQLx. |
| S10 | Schema changes are *expand then contract*: add the new column, deploy, migrate data, remove the old column in a later release. The previous app version must still work on the new schema, so rolling back is a code change, not a database restore. | Rule in ARCHITECTURE.md and decision record 0003. |
| S11 | Changing a synced table (books, members, loans) needs a matching Postgres migration in the same change. | Contract test (§12). |
| S12 | Never copy a live database with `cp`. Use `litestream restore` or `VACUUM INTO`. Never delete `library.db` without also deleting its `-wal` and `-shm` files. | RUNBOOK; `restore.sh` refuses stray `-wal`/`-shm` files. |
| S13 | Exactly one Litestream per database, and a unique `NODE_ID` per node. Two writers to the same S3 path can make the backup impossible to restore. | `restore.sh` validates `NODE_ID`; the worker's lock (§6.6) detects duplicate ids. |
| S14 | Backups are tested by restoring them, not by checking that files exist. | `scripts/restore-drill.sh` (§7.3) and the e2e test. |
| S15 | The database is checked for corruption after every restore and on demand. | `litestream restore -integrity-check quick`; `library-web check-db`. |
| S16 | Checkpointing is left to SQLite's default (every 1000 pages). Litestream 0.5 copes with app checkpoints. At very high write rates, tens of thousands of transactions a second, set `wal_autocheckpoint = 0` and let Litestream checkpoint. | Note in ARCHITECTURE.md. |

`crates/sqlite` public API:
- `connect_options(url, create_if_missing) -> SqliteConnectOptions`, with S2–S5 applied.
- `open_pool(url, max_connections, create_if_missing) -> SqlitePool`.
- `check(pool) -> CheckReport`, which runs `PRAGMA integrity_check` and
  `PRAGMA foreign_key_check`.

The web app keeps its pool of 8 connections. The worker uses 2.

## 6. The change log and the sync worker

### 6.1 Capturing changes: a change log written by triggers

A new SQLite migration adds a `change_log` table, plus triggers on `books`, `members` and
`loans`. Because a trigger runs inside the same transaction as the write that fired it,
**a change is logged if and only if it is committed**. No app code changes, and no write
path can forget to log. This is the *transactional outbox* pattern, done by the
database.

```sql
CREATE TABLE change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- AUTOINCREMENT: a number is never reused
    table_name TEXT    NOT NULL,
    row_id     INTEGER NOT NULL,
    op         TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
                                                   -- for people reading the log; the worker
                                                   -- does not need it
    token      BLOB    NOT NULL DEFAULT (randomblob(8)),
                                                   -- random fingerprint, see §6.4
    created_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER))
                                                   -- milliseconds, used to measure lag
);

CREATE TRIGGER books_logged_insert AFTER INSERT ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'insert');
END;
CREATE TRIGGER books_logged_update AFTER UPDATE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'update');
    -- if the id itself changed, the old id is gone:
    INSERT INTO change_log (table_name, row_id, op)
        SELECT 'books', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER books_logged_delete AFTER DELETE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', OLD.id, 'delete');
END;
-- The same three triggers for members and loans.
```

- The log records **which row changed, not what it now contains.** The worker reads the
  row's current values when it ships. Adding a column to `books` therefore needs no
  trigger change.
- Deleting a book or member cascades to its loans. SQLite fires triggers for cascaded
  deletes; this was checked with sqlite3 3.45, and a test keeps it true.
- `users` and `sessions` have no triggers (assumption A4).

### 6.2 The Postgres side

`migrations/postgres` creates three kinds of tables.

**Mirror tables** have the same columns as SQLite, plus `node_id` and `synced_at`:
```sql
CREATE TABLE books (
    node_id        TEXT   NOT NULL,
    id             BIGINT NOT NULL,
    title          TEXT   NOT NULL,
    author         TEXT   NOT NULL,
    isbn           TEXT,
    published_year BIGINT,
    total_copies   BIGINT NOT NULL,
    created_at     BIGINT NOT NULL,   -- kept as Unix seconds, exactly as in SQLite
    updated_at     BIGINT NOT NULL,
    synced_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);
-- members and loans follow the same pattern.
```
- The only constraint is the primary key: no foreign keys, no unique email or ISBN.
- The node already enforces those rules. A copy that re-checks them would make batch
  order matter and add failure modes for no gain.
- Values are copied exactly. SQLite integers are 64-bit, so they become `BIGINT`.

**One checkpoint per node:**
```sql
CREATE TABLE sync_checkpoints (
    node_id    TEXT PRIMARY KEY,
    epoch      INTEGER NOT NULL,      -- increases on every full copy (§6.5)
    last_seq   BIGINT  NOT NULL,      -- highest change_log seq already applied here
    last_token BYTEA,                 -- token of that change_log row (NULL when last_seq = 0)
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

**The archive** for rows replaced by a full copy:
```sql
CREATE TABLE sync_archive (
    id          BIGSERIAL PRIMARY KEY,
    node_id     TEXT    NOT NULL,
    epoch       INTEGER NOT NULL,     -- the epoch these rows belonged to
    table_name  TEXT    NOT NULL,
    row_data    JSONB   NOT NULL,
    archived_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

### 6.3 One sync cycle

The worker keeps its node's checkpoint in memory after reading it once per session. It
is the only writer of that checkpoint (§6.6). It then repeats this cycle:

1. **Read**, in one SQLite read transaction, so it all comes from one moment in time:
   1. Check that the checkpoint is still valid (§6.4). If not, do a full copy (§6.5)
      instead.
   2. Read up to `BATCH_SIZE` (default 500) change_log rows with `seq > last_seq`, in
      order.
   3. Reduce them to the distinct `(table, row_id)` pairs. That is the *coalescing*: 50
      updates to one book become one entry.
   4. Read those rows' current values:
      `SELECT … FROM books WHERE id IN (SELECT value FROM json_each(?))`.
      A row that is in the log but no longer in its table was deleted.
2. **Write**, in one Postgres transaction:
   1. For each table, insert or update all present rows in **one statement**:
      `INSERT … SELECT $node, * FROM UNNEST($2::bigint[], $3::text[], …)
      ON CONFLICT (node_id, id) DO UPDATE SET …`.
   2. For each table, remove the deleted rows:
      `DELETE … WHERE node_id = $1 AND id = ANY($2)`.
   3. Move the checkpoint to the batch's last `seq` and that row's `token`.
   4. Commit.
3. **Prune**: in a short SQLite write transaction,
   `DELETE FROM change_log WHERE seq < new_last_seq`. The row *at* `last_seq` is kept on
   purpose (§6.4).
4. **Wait**:
   - If the batch was full, start the next cycle immediately.
   - If it had changes, wait 200 ms.
   - If it was empty, wait twice as long as last time, from 200 ms up to 2 s.

**Why nothing is lost or duplicated:**
- Postgres data and the checkpoint commit together, so they cannot disagree.
- If the worker dies after the Postgres commit but before the prune, the next cycle
  starts from the saved checkpoint. It skips rows already sent, and the prune catches
  up.
- Upserts and deletes by `(node_id, id)` give the same result if they are repeated.
  Shipping current values also means a replay can never put an old value back.

**Why it is efficient:**
- A web request pays only for one extra small insert, inside a transaction it already
  had.
- The worker uses one Postgres round trip per table per batch, no matter how many rows
  are in it.
- Rows that change often are sent once per batch.
- When nothing is happening, the worker makes one cheap indexed query every 2 s.

**Why polling:** the web app and the worker are separate processes, and SQLite cannot
signal another process. Polling the indexed `change_log` costs microseconds.

### 6.4 Detecting a restored database: tokens

**The danger.** A node is restored from an S3 backup that is slightly older than what
Postgres has already received. The restored change_log has a lower highest `seq`, so new
writes reuse `seq` numbers that Postgres's checkpoint already counts as shipped. Trusting
the checkpoint would skip those writes forever.

**The check.** Every change_log row gets a random 8-byte `token`. The checkpoint stores
the `seq` and the `token` of the last row it shipped. At the start of every cycle, inside
the read transaction, the worker checks:

> The checkpoint is **valid** if `last_seq = 0`, or if change_log has a row with that
> `seq` **and** the same `token`.

How each situation comes out:

| Situation | change_log row at `last_seq` | Result |
|---|---|---|
| Normal running | present, same token (never pruned) | valid |
| Restore from a backup newer than the checkpoint (the usual case, because Litestream is about as fast as the worker) | present, same token | valid; carry on |
| Restore from a backup older than the checkpoint | missing | **invalid → full copy** |
| …and the app has already written new rows that reused the numbers | present, **different** token | **invalid → full copy** |
| Postgres itself was restored to an older point | pruned long ago, so missing | **invalid → full copy** |
| Brand-new Postgres, no checkpoint | n/a | full copy (bootstrap) |

The worker never prunes the checkpoint row itself, and `AUTOINCREMENT` never reuses a
number within one database history. So in normal running the check always passes. It is
one indexed lookup per cycle.

### 6.5 Full copy: bootstrap and resync

The same code handles the first run and recovery after a restore:

1. **Read**, in one SQLite read transaction:
   - every row of the synced tables;
   - the highest `seq` ever issued, `S`, from `sqlite_sequence` (0 if none);
   - the `token` of row `S`.
2. **Write**, in one Postgres transaction:
   1. Copy the node's current mirror rows into `sync_archive`, if there are any, tagged
      with the old epoch.
   2. Delete them from the mirror.
   3. Insert the snapshot, in chunks of 1000 rows.
   4. Set the checkpoint to `(epoch + 1, S, token)`. Epoch 1 on bootstrap.
   5. Commit.
3. **Prune** `seq < S`.

Then normal cycles continue from `S`. Anyone reading Postgres sees either the old copy or
the new one, never a mix.

Every resync is logged as a warning and counted in `sync_resyncs_total`, which triggers
an alert. A resync after a restore means the node lost some writes. Those rows are in
`sync_archive` for a person to inspect; RUNBOOK §"After a resync" explains how.

**Limit:** the copy reads every row while holding one read transaction. That is fine for
this example's size. A large database would copy in chunks, which ARCHITECTURE.md notes
as future work.

### 6.6 One worker per node: a Postgres lock

At startup the worker opens **one** Postgres connection and takes a session advisory
lock on a key derived from its `NODE_ID`: `pg_try_advisory_lock(hashtext('sync:' ||
node_id))`.

- If another worker already holds it, for example because two VMs were given the same
  `NODE_ID`, this worker logs an error naming the node and retries every 30 s. It never
  ships anything.
- All of the worker's Postgres writes go through **that same connection**. If the
  connection drops, Postgres releases the lock, and the worker's writes fail too. The
  worker then reconnects, takes the lock again and re-reads the checkpoint before doing
  anything.

### 6.7 Schema handshake

`web` runs the SQLite migrations. During a rolling deploy the new worker can start
before the new `web` has migrated.

- The worker embeds the same `migrations/sqlite` list but never runs it.
- Before shipping, it checks that every migration it knows about appears in
  `_sqlx_migrations`. Until they all do, it logs "waiting for web to migrate" and checks
  again every second.
- The worker runs the Postgres migrations itself. SQLx takes a Postgres lock while
  migrating, so three workers starting at once is safe.
- Postgres changes also follow expand-then-contract (S10), because an old worker on
  another node may still be running.

### 6.8 Failures inside the worker

| Problem | Behaviour |
|---|---|
| Postgres unreachable or a query fails | End the session: drop the connection and lock. Retry with waits doubling from 1 s to 30 s. The change log keeps growing in SQLite. The app is not affected. |
| SQLite busy | `busy_timeout` waits up to 5 s. If the wait runs out, that cycle fails and is retried. |
| `library.db` does not exist yet | Wait and retry every second; `web` creates it on a fresh node. |
| SIGTERM | Finish the current cycle, then exit 0. |
| A bug or bad data in one row | The batch fails and is retried. It keeps failing, the lag alert fires, and the log names the batch. The worker never skips a change. |

Worker subcommands:
- `sync-worker`: run (the default).
- `sync-worker status`: print node, epoch, `last_seq`, changes still to send, and the
  age of the oldest one. Used in the RUNBOOK.

### 6.9 Adding a synced table later

Each step is checked by a test:
1. A SQLite migration with the table and its three triggers.
2. A Postgres migration with the mirror table.
3. One entry in the worker's table list (`apps/sync-worker/src/tables.rs`).

If Postgres should one day keep history, store the row's values in the change log
(`json_object(...)` in the trigger) and append them to a history table. That is out of
scope now.

## 7. Backups with Litestream, and the restore gate

### 7.1 Litestream setup

- Version pinned to **0.5.17**. The 0.5 series changed the backup format to LTX and
  early 0.5.0 had bugs, so the version is pinned exactly and tested by real restores.
- Our image is the official `litestream/litestream:0.5.17` (Debian, so it has a shell),
  plus:
  - `litestream.yml`
  - `restore.sh`
  - `/data` owned by uid 10001
  - `USER 10001`
- `litestream.yml`:
  - one database, `/data/library.db`
  - one S3 replica: bucket, endpoint and keys from environment variables, path
    `${NODE_ID}`, path-style addressing for Garage
  - metrics on `:9090`
  - retention, so old backup files are deleted
- The exact key names are checked against 0.5.17's documentation and `litestream.yml`
  reference while implementing.

### 7.2 The restore gate (`restore.sh`)

Runs before anything else on every start:

1. Check that `NODE_ID` matches `^[a-z0-9][a-z0-9-]{0,39}$`, else exit 1.
2. If `library.db` exists: print "database present" and exit 0. This is the normal
   redeploy.
3. If `library.db` is missing but a `-wal` or `-shm` file or Litestream's
   `.library.db-litestream` folder is left over: exit 1, explaining that someone deleted
   only part of the database (S12). A person must look.
4. Run `litestream restore -if-replica-exists -integrity-check quick /data/library.db`.
   If it fails, for example because S3 is unreachable, exit 1. **Never fall through to
   an empty start because S3 was down.**
5. If the database now exists: print "restored" and exit 0.
6. No backup exists. If `ALLOW_FRESH_DB=true`, print "starting with a new empty
   database" and exit 0. Otherwise exit 1 with:

   > No database and no backup for node `<id>` at `<bucket>/<id>`. If this is a
   > brand-new node, set ALLOW_FRESH_DB=true for its first start.

   This catches a wrong bucket or a mistyped `NODE_ID`. Without it, the node would come
   up empty and Litestream would start a new backup.

Because `web` waits for `restore` to succeed, a failed gate means the app does not
start. That is the safe outcome.

The implementation must confirm two things about Litestream 0.5.17:
- that an unreachable S3 makes `litestream restore -if-replica-exists` exit non-zero.
  If it does not, `restore.sh` first checks S3 with a separate listing command;
- that `litestream` starts cleanly before `web` has created the file on a fresh node.
  If it does not, the gate creates the empty file in WAL mode.

### 7.3 Restore drill (`scripts/restore-drill.sh <node>`)

1. Restores the node's latest backup from S3 into a temporary file, without touching the
   running node.
2. Runs `library-web check-db` on the copy.
3. Prints the row counts of the copy next to the live node's counts.

It exits non-zero if the check fails. In production it would run on a schedule. The
RUNBOOK says weekly.

## 8. Monitoring

| Source | What it exposes |
|---|---|
| `web` | `GET /api/health` → `{status, node_id, version}`; it also runs `SELECT 1` on the database. |
| `litestream` | `:9090/metrics`: database and WAL size, replication progress (Litestream's own metrics). |
| `sync-worker` | `:9100/health`: 200 if a cycle succeeded in the last 60 s. `:9100/metrics` in Prometheus format via the `prometheus-client` crate: `sync_pending_changes`, `sync_lag_seconds` (age of the oldest unsent change), `sync_last_success_timestamp_seconds`, `sync_batches_total`, `sync_rows_shipped_total`, `sync_errors_total`, `sync_resyncs_total`, `sync_epoch`. |

`deploy/monitoring/alerts.yml`:

| Alert | Condition | Meaning |
|---|---|---|
| `SyncLagHigh` | `sync_lag_seconds > 60` for 5 min | Postgres is falling behind: Postgres is down, or there is a bad row. |
| `SyncResync` | `increase(sync_resyncs_total[1h]) > 0` | A node was restored and may have lost writes. Check `sync_archive`. |
| `SyncWorkerDown` / `LitestreamDown` | `up == 0` for 2 min | A container is down. |
| `LitestreamStalled` | Replication progress has not moved for 5 min while the WAL keeps growing | The S3 backup has stopped. The exact metric is taken from 0.5.17's real output. |
| `WalTooLarge` | WAL size above 256 MB for 15 min | Checkpoints are blocked, often by a long read transaction. |

Disk-space alerts need the host's own metrics, which are out of scope. The RUNBOOK lists
them as a production must-have.

Metrics and health ports are reachable only inside the Docker network, never published
on the host in `node.yml`. Prometheus reaches them through the shared network of the
local cluster.

## 9. Configuration

Everything is set by environment variables. `node.env.example` documents each one.

| Variable | Used by | Meaning |
|---|---|---|
| `NODE_ID` | all | Unique node name, also the S3 folder and the Postgres `node_id`. |
| `HTTP_PORT` | compose | Host port for `web` (4001–4003 locally). |
| `DATABASE_URL` | web, sync-worker | `sqlite:///data/library.db` (set in the image). |
| `LITESTREAM_BUCKET`, `LITESTREAM_ENDPOINT`, `LITESTREAM_REGION`, `LITESTREAM_ACCESS_KEY_ID`, `LITESTREAM_SECRET_ACCESS_KEY` | restore, litestream | Where the backup goes. |
| `ALLOW_FRESH_DB` | restore | `true` only for a brand-new node's first start. |
| `POSTGRES_URL` | sync-worker | The only place Postgres credentials appear. |
| `BATCH_SIZE` | sync-worker | Default 500. |
| `COOKIE_SECURE`, `SESSION_TTL_HOURS`, `LOG_FORMAT`, `RUST_LOG` | web | As in the library app. |

The local dev keys for Garage and the Postgres password are committed in
`deploy/compose/local/*.env` and labelled as local-only. Real values never go in git.

## 10. Local development and the local cluster

**Coding, with no containers.** `make dev` runs `web` against `data/dev.db`, with Vite
for React, exactly like the library app. The worker can run locally too:
```
POSTGRES_URL=… NODE_ID=dev cargo run -p sync-worker
```

**Tests that need Postgres:**
- `make test` starts a throwaway `postgres:18.6` container on `127.0.0.1:55432`
  (`scripts/test-postgres.sh`) and passes its address to the tests as
  `TEST_POSTGRES_URL`.
- Any other Postgres works if you set that variable yourself.
- Without it, the worker tests fail with a message saying how to set it. They never
  silently skip.

**The cluster: `make up`.**
1. Builds the three images, tagged `sqlite-production/{web,sync-worker,litestream}:dev`.
2. Starts `central.yml` as compose project `central`:
   - `postgres:18.6`, published on `127.0.0.1:5433`;
   - `dxflrs/garage:v2.4.1`;
   - `prom/prometheus:v3.13.4` on `127.0.0.1:9090`.
3. Runs `deploy/garage/init.sh`, which is idempotent. It sets up the single-node layout,
   creates the bucket and imports the fixed dev key.
4. Starts `node.yml` three times as projects `node1`, `node2` and `node3`, each with its
   own env file and `local/node.override.yml`. They serve on 4001, 4002 and 4003.

This is the same `node.yml` a real VM uses. The override file only joins the local
shared network.

Other Makefile targets:

| Target | What it does |
|---|---|
| `down` | Stop everything; volumes are kept. |
| `reset` | Also delete the volumes. Asks first. |
| `logs` | Follow the logs. |
| `psql` | Open psql on the cluster's Postgres. |
| `restore-drill NODE=node2` | Run the restore drill (§7.3). |
| `e2e` | Run the end-to-end test (§12). |
| `check` | Every CI gate. |
| `types` | Regenerate the TypeScript types. |
| `sqlx-prepare` | Refresh the committed offline query data. |
| `seed NODE=node1` | Seed sample data on a node. |
| `user NODE=node1` | Create a staff account on a node. |

Garage replaces MinIO because MinIO's community edition is archived and stopped
publishing images in October 2025 (decision record 0006).

## 11. Failures and what happens

| Event | What happens | Data at risk | Covered by |
|---|---|---|---|
| Redeploy or restart | About 1–2 s of downtime. The volume keeps the database. `restore` sees the file and does nothing. | none | e2e |
| VM or disk lost | New VM, empty volume: `restore` pulls the backup from S3, then the worker checks its checkpoint (§6.4). | up to about 1 s of the last writes (Litestream's interval) | e2e (wipe a node's volume) |
| …and Postgres had received writes the backup lacks | Worker detects it, resyncs, archives the lost rows, alert fires. | Those writes are gone from the node but kept in `sync_archive`. | worker integration test |
| Node misconfigured (wrong bucket or `NODE_ID`) with an empty disk | `restore` refuses; the app does not start. | none | e2e |
| S3 down at runtime | Litestream retries. The WAL grows on local disk. The app is not affected. `LitestreamStalled` alert. | The window grows until S3 returns. | RUNBOOK |
| S3 down while restoring | `restore` fails; the app does not start until S3 is back. | none | RUNBOOK; the gate is checked in implementation (§7.2) |
| Postgres down | Worker retries. The change log grows. The app is not affected. `SyncLagHigh` alert. When Postgres returns, the worker catches up. | none | e2e |
| Postgres restored to an older point | Checkpoint is invalid, so a full copy of every node runs automatically. | none | worker integration test |
| Two nodes with the same `NODE_ID` | The second worker cannot take the lock and logs an error. The Litestream collision is a misconfiguration; the RUNBOOK says how to spot it. | backup path | worker integration test, RUNBOOK |
| Bad migration | Fix forward with a new migration. Last resort: restore to a time before the deploy (`litestream restore -timestamp`). | writes after that time | RUNBOOK |
| Corruption (rare: bad disk) | `check-db` fails, then restore from S3. | depends on when it started | RUNBOOK, restore drill |

Not handled: zero-downtime deploys, and moving a user to another node. See §13.

## 12. Testing

| Layer | What | Needs |
|---|---|---|
| Unit | `crates/sqlite`: every pragma is set on every connection. Worker pure logic: coalescing a batch, the wait schedule, checkpoint validity rules. | nothing |
| Web integration | The library app's existing tests, moved over. New: every insert, update and delete on books, members and loans writes exactly the expected change_log rows. Cascaded loan deletes are logged. Writes to users and sessions are not. An id change logs a delete for the old id. | nothing (`#[sqlx::test]`, SQLite) |
| Worker integration | Real SQLite file plus real Postgres, one fresh Postgres database per test. Cases: bootstrap; inserts, updates and deletes; 100 updates sent as 1 row; crash between commit and prune (no duplicates, same end state); restore from an older copy of the file, which resyncs and archives; restore from a newer copy, which does not resync; a second worker cannot take the lock; waiting for the web migrations; Postgres disappearing then returning, with catch-up. Every test ends by comparing all rows in SQLite and Postgres. | `TEST_POSTGRES_URL` |
| Contract | For each synced table, the SQLite columns equal the Postgres mirror columns minus `node_id` and `synced_at`, and the triggers exist. Catches S11. | `TEST_POSTGRES_URL` |
| End-to-end (`scripts/e2e.sh`, `make e2e`) | Starts the full cluster, then: (1) writes on each node, which appear in Postgres under the right `node_id`; (2) stop node2, delete its volume, start it, and its data is back, with no resync; (3) an empty volume with a `NODE_ID` that has no backup and no `ALLOW_FRESH_DB`, and `restore` refuses; (4) stop Postgres, write, start Postgres, and it catches up; (5) restore drill passes. Then tears everything down. | Docker |
| Frontend | The React tests copied from the library app. | Node |

**CI** (`.github/workflows/ci.yml`):
- Rust fmt, clippy and tests, with a Postgres service container.
- Generated TypeScript types are up to date.
- React lint, typecheck, tests and build.
- Docker images build.
- e2e as a separate job.

There is no git remote yet, so CI runs once the repo is pushed. `make check` runs the
same gates locally.

## 13. Documents

- **ARCHITECTURE.md**: the picture in §2, data flow for a write, the guarantees
  (backup delay, what Postgres is and is not), the SQLite rules in §5, and future work
  (chunked full copy, history table, zero-downtime deploys).
- **RUNBOOK.md**. Each procedure gives steps, commands, and how to confirm it worked:
  - add a node
  - deploy a new version
  - replace a lost VM
  - restore to a point in time
  - Postgres down
  - S3 down
  - after a resync (reading `sync_archive`)
  - duplicate `NODE_ID`
  - disk filling up
  - running the restore drill
  - checking a database
- **decisions/** (short records: context, decision, consequences):
  1. 0001: SQLite as each node's primary database.
  2. 0002: one container per job.
  3. 0003: expand-and-contract migrations.
  4. 0004: change log written by triggers; sending current row values, not change
     payloads.
  5. 0005: tokens and epochs to detect restores.
  6. 0006: Garage instead of MinIO for local S3.
- **README.md**: what this is, quick start (`make up`, open 4001–4003), and the one-line
  job of every folder.

## 14. Out of scope

- A load balancer and sticky routing: each node has its own port. Production would map
  each branch to its node, for example by subdomain.
- Moving users between nodes, and any data shared between nodes.
- Copying anything from Postgres back to SQLite. Recovering a node from Postgres alone
  is a manual job.
- Grafana and log shipping.
- Host metrics (disk, CPU).
- TLS, a secrets manager, separate Postgres roles for migration and runtime: noted in
  the RUNBOOK as production to-dos.
- Ansible, Terraform and image registries: you copy `node.yml` and an env file to a VM.
- Zero-downtime deploys.
- The Leptos frontend.

## 15. Versions

Each version is pinned exactly where the tool allows it.

| Item | Version |
|---|---|
| Rust toolchain | stable (1.96 at time of writing), edition 2024 |
| Axum | 0.8 |
| SQLx | 0.9 (SQLite and Postgres features; the Postgres feature only in sync-worker) |
| tower-http | 0.7 |
| prometheus-client | 0.25 |
| React / Vite / TanStack Query | as in the library app |
| Litestream | 0.5.17 |
| Garage | v2.4.1 |
| Postgres | 18.6 |
| Prometheus | v3.13.4 |
| Node (frontend build) | 22 |

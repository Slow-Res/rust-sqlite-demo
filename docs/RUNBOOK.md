# Runbook

Step-by-step procedures. Each has three parts: **When** (the symptom or alert),
**Steps**, and **Check it worked**. The `runbook:` line of each alert in
[alerts.yml](../deploy/monitoring/alerts.yml) points at one of these headings.

## Conventions

On a VM, every command runs next to `node.yml` and `node.env`. `<node>` is the
node's `NODE_ID`, and the same name is the Docker Compose project, so the volume is
called `<node>_data`:

```bash
docker compose -p <node> -f node.yml --env-file node.env <command>
```

On the local cluster, the same commands go through the helper:
`scripts/cluster.sh exec node2 sync-worker sync-worker status` runs a command in the
`sync-worker` service of `node2`, and `make logs NODE=node2` follows its logs.

Two things are always true:

- The app never needs Postgres or S3 to serve requests.
- Never copy a live database with `cp`, and never delete `library.db` without its
  `-wal` and `-shm` files.

| Alert | Section |
|---|---|
| `SyncLagHigh`, `SyncWorkerDown` | [Postgres down](#postgres-down) |
| `SyncResync` | [After a resync](#after-a-resync) |
| `LitestreamDown`, `LitestreamStalled` | [S3 down](#s3-down) |
| `WalTooLarge` | [Disk filling up](#disk-filling-up) |

## Add a node

**When:** a new branch needs its own node.

**Steps**

1. Pick a `NODE_ID` that no other node has, ever: 1-40 characters of `a-z`, `0-9`
   and `-`, starting with a letter or digit. It is the node's S3 folder and its
   `node_id` in Postgres.
2. Copy `node.yml` and `node.env.example` to the VM. Rename the env file to
   `node.env` and fill it in: `NODE_ID`, `HTTP_PORT`, the `LITESTREAM_*` settings,
   `POSTGRES_URL` and the image names.
3. Set `ALLOW_FRESH_DB=true` in `node.env`. A node with an empty disk and no backup
   refuses to start otherwise (see [Database file missing](#database-file-missing-but-its-wal-is-still-there)).
4. Start it (step 7 adds the metrics file):

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env up -d
   ```

5. Wait until the node is healthy (below), then create a staff account:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env exec web library-web create-user admin
   ```

6. Set `ALLOW_FRESH_DB=false` in `node.env` and run the `up -d` command again.
   Leaving it `true` would let a later start with a wrong bucket or a mistyped
   `NODE_ID` come up empty and start a new backup.
7. Make the node's metrics reachable by Prometheus. `node.yml` publishes no
   ports, so on a VM also copy [node.metrics.yml](../deploy/compose/node.metrics.yml),
   set `METRICS_BIND_ADDR` in `node.env` to the VM's private address, and start the
   node with both files:

   ```bash
   docker compose -p <node> -f node.yml -f node.metrics.yml --env-file node.env up -d
   ```

   Then add `<METRICS_BIND_ADDR>:9090` (Litestream) and `<METRICS_BIND_ADDR>:9100`
   (worker) to [prometheus.yml](../deploy/monitoring/prometheus.yml). These ports
   have no authentication: they must be reachable only from Prometheus (a firewall
   rule or a private network), never from the internet. Use the same two `-f` files
   for every later `compose` command on that node.

If the first start fails (for example S3 is not reachable yet):

- On a VM, keep `ALLOW_FRESH_DB=true` until the first start has succeeded, then
  retry. A failed first start can leave the volume behind, and the gate refuses an
  empty disk when the setting is `false`.
- On the local cluster, `scripts/cluster.sh up` sets `ALLOW_FRESH_DB=true` only when
  the node's volume does not exist yet
  ([cluster.sh](../scripts/cluster.sh#L32)). If the first `make up` failed (for
  example Garage was not ready), the volume already exists, so the next `make up`
  treats the node as not fresh and the restore gate refuses the empty disk. Run
  `make reset` (it asks first and deletes all local cluster data), then `make up`.

**Check it worked**

```bash
curl -fsS http://127.0.0.1:<HTTP_PORT>/api/health
docker compose -p <node> -f node.yml --env-file node.env logs restore
docker compose -p <node> -f node.yml --env-file node.env exec sync-worker sync-worker status
```

- Health returns `"status":"ok"` and the node's id.
- The restore log says `starting with a new empty database (ALLOW_FRESH_DB=true)`.
- `status` shows `epoch: 1` and `waiting to ship: nothing` once the first copy is done.
- Within a minute, `SELECT * FROM sync_checkpoints WHERE node_id = '<node>'` in
  Postgres returns a row.

## Deploy a new version

**When:** a new release of the web app or the worker.

**Steps**

1. Build or pull the new images and set their tags in `node.env`
   (`WEB_IMAGE`, `WORKER_IMAGE`, `LITESTREAM_IMAGE`).
2. Do one node at a time. If the release changes the columns of a synced table
   (see [Adding a column to a synced table](ARCHITECTURE.md#adding-a-column-to-a-synced-table)),
   first stop the worker, so the old worker cannot ship rows without the new column:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env stop sync-worker
   ```

   Then, on that node:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env up -d
   ```

   The web container migrates SQLite as it starts, and `up -d` starts the new worker
   after it, which ships current row values including the new column.

   Compose recreates only the containers whose image changed. Expect about 1-2 s of
   downtime for that node's users.
3. Watch the web app, then the worker, before moving on:

   ```bash
   curl -fsS http://127.0.0.1:<HTTP_PORT>/api/health
   docker compose -p <node> -f node.yml --env-file node.env exec sync-worker sync-worker status
   ```

4. Repeat for the next node.

Migration rules (full reasoning in [decision 0003](decisions/0003-expand-and-contract-migrations.md)):

- Expand, then contract. Add the new column in one release. Use it in the next.
  Remove the old one in a later release. The previous version must still work on the
  new schema, so rolling back is deploying the old image.
- Both migrators accept a database that has applied a migration they do not know
  (`ignore_missing`), so the previous image still starts after a newer release ran
  an expand migration. That is what makes rolling back an image deploy.
- `library-web` runs the SQLite migrations when it starts. The worker waits and logs
  `waiting for library-web to apply the SQLite migrations` until they are all there.
- The worker runs the Postgres migrations when it connects. Another node's older
  worker may still be running, so Postgres changes are expand-then-contract too.
- Deploy the worker image and the web image of the same release together. A worker
  that finds a table it does not know stops the cycle instead of skipping the change.
- Never edit a migration that has been merged. Fix forward with a new one.

**Check it worked**

- `/api/health` returns `"status":"ok"` with the new version.
- `status` shows `waiting to ship: nothing` after a few seconds.
- The worker's own health returns 200:
  `docker compose -p <node> -f node.yml --env-file node.env exec sync-worker curl -fsS http://127.0.0.1:9100/health`.

## Replace a lost VM

**When:** the VM or its disk is gone.

**Steps**

1. Make sure the old VM is really off. Two live nodes with the same `NODE_ID` are
   dangerous (see [Duplicate NODE_ID](#duplicate-node_id)).
2. Start a new VM with Docker. Copy `node.yml` and the node's **same** `node.env`.
3. Keep `ALLOW_FRESH_DB=false`. The node must restore, never start empty.
4. Start it:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env up -d
   ```

   The `restore` container pulls the newest backup from S3 and checks it. If S3 is
   unreachable it fails and the app does not start; fix S3 and run the command again.
5. Point the branch's users at the new VM.

You lose about 1 s of the last writes before the disk was lost.

**Check it worked**

- `logs restore` says `restored /data/library.db from the backup`.
- `/api/health` is `ok`, and the books you expect are there.
- `sync-worker status` shows a normal position. If the backup was older than what
  Postgres had, the worker does a resync and `SyncResync` fires; follow
  [After a resync](#after-a-resync).

## Restore to a point in time

**When:** a bad migration or bad data, and fixing forward is not possible. This
throws away every write after the chosen time. Litestream keeps 3 days of history
([litestream.yml](../deploy/docker/litestream/litestream.yml)), which is how far back
you can go.

**Steps**

1. Stop the node. The volume stays:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env down
   ```

2. Delete the database **and** its `-wal`, `-shm` and Litestream folder, all four.
   Removing only `library.db` would leave a WAL that SQLite replays into the restored
   file. This runs in the `litestream` service, so it uses the node's own image and
   volume:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env run --rm --no-deps \
     --entrypoint sh litestream \
     -c 'rm -rf /data/library.db /data/library.db-wal /data/library.db-shm /data/.library.db-litestream'
   ```

3. Restore to the time you want (UTC) through the restore image:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env run --rm --no-deps \
     --entrypoint litestream litestream \
     restore -config /etc/litestream.yml -timestamp 2026-10-11T09:00:00Z \
     -integrity-check quick /data/library.db
   ```

4. Start the node: `docker compose -p <node> -f node.yml --env-file node.env up -d`.

Expect a resync: the restored log is older than Postgres's checkpoint. Read
[After a resync](#after-a-resync) for the rows that were lost.

**Check it worked**

- `check-db` passes (see [Check a database](#check-a-database-for-corruption)) and the
  data is as it was at that time.
- `sync-worker status` shows a higher `epoch` than before.
- Litestream is backing up again. After one new write on the node, the node's
  `litestream_replica_operation_total{operation="PUT"}` rises in Prometheus.
- After a quiet moment (no writes for about 10 s), `scripts/restore-drill.sh <node> --strict`
  passes, so the new write is in the backup. This was tested on the local cluster: after
  the restore Litestream carried on without an extra step, and the strict drill passed.

## After a resync

**When:** the `SyncResync` alert fires, or the worker logs
`the checkpoint did not match this database (restored from an older backup?)`. The node
was restored from an older backup, or Postgres was restored. The worker copied the
whole node again, bumped the node's epoch, and put the rows Postgres used to have in
`sync_archive`, tagged with the old epoch.

The alert reads the gauge `sync_last_resync_timestamp_seconds`: it is 0 until the
worker detects a resync, then the unix time of it. `SyncResync` fires while that
time is less than an hour old, so it does not depend on Prometheus having scraped an
earlier value. The gauge lives in the worker process: a worker that restarts starts
at 0 again, so a resync is shown for up to an hour only if the worker stays up. To
check an older one, compare the `sync_epoch` metric, or the `epoch` line of
`sync-worker status`, with what you expect. Epoch 1 is the first sync, and every
resync adds 1.

**Steps**

1. List what the archive holds for the node:

   ```sql
   SELECT node_id, epoch, table_name, count(*), max(archived_at)
   FROM sync_archive GROUP BY 1, 2, 3 ORDER BY max(archived_at) DESC;
   ```

   The rows of a resync have the epoch from before it. If `status` now says
   `epoch: 2`, look at `epoch = 1`.
2. List the rows that only existed in the lost history: archived rows that the
   node's current data does not have (same id and same values):

   ```sql
   SELECT a.table_name, a.row_data FROM sync_archive a
   WHERE a.node_id = 'node2' AND a.epoch = 1 AND NOT EXISTS (
     SELECT 1 FROM (
       SELECT 'books' AS t, node_id, id, to_jsonb(b) - 'synced_at' AS j FROM books b
       UNION ALL SELECT 'members', node_id, id, to_jsonb(m) - 'synced_at' FROM members m
       UNION ALL SELECT 'loans', node_id, id, to_jsonb(l) - 'synced_at' FROM loans l
     ) cur
     WHERE cur.t = a.table_name AND cur.node_id = a.node_id
       AND cur.id = (a.row_data->>'id')::bigint
       AND cur.j = a.row_data - 'synced_at');
   ```

   Change `node2` and `1` to the node and epoch you are looking at.
3. Decide with the branch whether to enter those rows again in the app. They come
   back as new rows with new ids.

Rows are kept in `sync_archive` for ever. Nothing deletes them.

**Check it worked**

- `status` shows the epoch you expect and `waiting to ship: nothing`.
- `sync_epoch` on the node matches `status`.

## Postgres down

**When:** the `SyncLagHigh` alert (the oldest unsent change is over 60 s old for 5
minutes) or `SyncWorkerDown`, or the worker logs `sync session failed; retrying`.

The app is not affected. The worker retries, waiting 1 s, then 2 s, doubling up to
30 s, and the change log grows in SQLite. When Postgres is back, the worker catches
up without help. It never skips a change.

**Steps**

1. Confirm the app is fine: `curl -fsS http://127.0.0.1:<HTTP_PORT>/api/health`.
2. Read the backlog from the worker's metrics. `sync-worker status` needs Postgres,
   so it cannot help here:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env exec sync-worker \
     curl -fsS http://127.0.0.1:9100/metrics | grep -E '^sync_(pending_changes|lag_seconds)'
   ```

3. Find the cause in `docker compose -p <node> -f node.yml --env-file node.env logs sync-worker`:
   Postgres unreachable, wrong password in `POSTGRES_URL`, or one bad row that makes
   every batch fail. Fix it. If you changed `POSTGRES_URL`, recreate the worker with
   `up -d`.
4. If the outage lasts days, watch the node's disk (see [Disk filling up](#disk-filling-up)).

If Postgres was **restored to an older point**, every node notices at its next cycle
and does a full copy, so `SyncResync` fires for every node. Nothing is lost: what
Postgres had before goes to `sync_archive`.

**Check it worked**

- `sync_lag_seconds` goes back to 0 and `status` says `waiting to ship: nothing`.
- The worker's health returns 200.
- `SELECT node_id, epoch, last_seq FROM sync_checkpoints` shows every node.

## S3 down

**When:** `LitestreamStalled` or `LitestreamDown` fires, or the `litestream`
container logs upload errors.

The app is not affected. Litestream keeps retrying and the WAL grows on local disk.
Until S3 is back the backup falls further behind: if the disk is lost now, every
write since the outage began is lost. A node that restarts during the outage with its
database on disk starts normally. A node with an **empty** disk fails its restore and
does not start, which is the safe outcome.

How `LitestreamStalled` works (see [alerts.yml](../deploy/monitoring/alerts.yml)):

- It fires when `litestream_txid` changed at least once in the last 30 minutes and
  the node had no successful upload in those 30 minutes: either
  `increase(litestream_replica_operation_total{operation="PUT"}[30m])` is 0, or no
  PUT series exists yet (for example Litestream restarted during the outage).
- Windows that contain Litestream's start-up jump of `litestream_txid` (from 0 to
  the current value) are ignored, so a node that has only just started and had no
  writes does not alert. A node that restarts during an outage can therefore take up
  to 30 minutes longer to alert.
- It must hold for 5 minutes (`for: 5m`), so expect the alert about 35 minutes
  after the last upload, a quiet node with a single write included.
- A live test showed that the PUT counter counts only successful uploads, and that
  `litestream_sync_error_count` stays 0 during an S3 outage, so that error counter is
  not a usable signal.
- `make alerts-test` runs the rule's test cases (`promtool`).

**Steps**

1. Look at the errors:
   `docker compose -p <node> -f node.yml --env-file node.env logs --tail 50 litestream`.
2. Fix the cause: the S3 service, the network, an expired key, a changed endpoint in
   `node.env`.
3. If you changed `node.env`, recreate the container with `up -d`.
4. While S3 is down, watch the WAL (see [Disk filling up](#disk-filling-up)).

**Check it worked**

- In Prometheus, `increase(litestream_replica_operation_total{operation="PUT"}[30m])`
  is above 0 for the node, and the alert clears.
- [Run the restore drill](#run-the-restore-drill) for the node. It proves the new
  backup restores.

## Duplicate NODE_ID

**When:** the worker logs `another sync-worker holds this node's lock; is NODE_ID used
twice? retrying`. It repeats every 30 s, `sync_errors_total` rises, and that worker's
health stays at 503 because it never ships anything.

Two nodes were given the same `NODE_ID`. The Postgres lock stops the second worker
from shipping, so Postgres is safe. The risk is to the backup: both Litestreams write
to the same S3 folder, `<bucket>/<NODE_ID>/`. Two writers on one path can make the
backup impossible to restore, and nothing detects that.

**Steps**

1. Decide which node is the duplicate (usually the newer one). Stop all of its
   containers: `docker compose -p <node> -f node.yml --env-file node.env down`.
2. Give it its own unique `NODE_ID` in `node.env`.
   - If it was a brand-new node with no data, delete its volume
     (`docker volume rm <project>_data`) and add it as a new node with
     `ALLOW_FRESH_DB=true` (see [Add a node](#add-a-node)).
   - If it holds real data, its new id gets its own S3 folder and its own full copy in
     Postgres. Start it and check it as in Add a node.
3. Check the original node's backup:
   `scripts/restore-drill.sh <original-node> [env-file]`.
4. If the drill fails, restore the original node to a time before the duplicate
   started (see [Restore to a point in time](#restore-to-a-point-in-time)).

**Check it worked**

- The error line stops in the original worker's log, and its health is 200.
- The restore drill passes for the original node.

## Disk filling up

**When:** the `WalTooLarge` alert (the WAL has been over 256 MB for 15 minutes), or
the VM's disk is nearly full. There are no host disk alerts here; see
[Production to-dos](#production-to-dos).

Common causes:

- A long read transaction. SQLite cannot reset the WAL while a reader is still using
  it, so the WAL keeps growing. Look for a stuck request or someone's open `sqlite3`
  shell on the file.
- Postgres down for days, so the change log keeps every change.
- S3 down, so Litestream cannot make progress.

**Steps**

1. See the sizes:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env exec web ls -l /data
   ```

   `library.db-wal` is the WAL. Prometheus has `litestream_wal_size`.
2. See how much the worker has not shipped. This needs Postgres to be up:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env exec sync-worker sync-worker status
   ```

   `waiting to ship` shows the number of changes and the age of the oldest. If it
   is large, go to [Postgres down](#postgres-down). When Postgres is down, use the
   `sync_pending_changes` metric instead.
3. For a stuck reader, close the shell or restart the web container:
   `docker compose -p <node> -f node.yml --env-file node.env restart web`. Once no
   reader holds the old WAL, the next checkpoint can reset it.
4. If S3 is the cause, go to [S3 down](#s3-down).
5. Never delete the `-wal` file to free space. It holds committed writes.

After the change log is pruned, `library.db` keeps its size on disk. SQLite reuses
the free space, so the file stops growing.

**Check it worked**

- `litestream_wal_size` falls and the alert clears.
- The VM has free space again (`df -h`).

## Database file missing but its WAL is still there

**When:** the `restore` container exits with `library.db is missing but ... exists:
part of the database was deleted. Refusing to start`, and the app does not start.

Someone deleted `library.db` but not all of `library.db-wal`, `library.db-shm` or
the `.library.db-litestream` folder. SQLite would replay the old WAL into a new file
([the gate](../deploy/docker/litestream/restore.sh#L26)), so a person must look first.

**Steps**

1. List the volume:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env run --rm --no-deps \
     --entrypoint sh litestream -c 'ls -la /data'
   ```

2. Decide where the data should come from. Usually it is S3.
3. To restore from S3, delete the leftover files too:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env run --rm --no-deps \
     --entrypoint sh litestream \
     -c 'rm -rf /data/library.db /data/library.db-wal /data/library.db-shm /data/.library.db-litestream'
   ```

4. Start the node. The gate now sees an empty disk and restores the newest backup.
   Keep `ALLOW_FRESH_DB=false`.

**Check it worked**

- `logs restore` says `restored /data/library.db from the backup`.
- [Check the database](#check-a-database-for-corruption) passes.

## Run the restore drill

**When:** weekly, and after any change to the S3 or Litestream settings. A backup
that has never been restored is not a backup.

**Steps**

Locally: `make restore-drill NODE=node2`. On a VM, from a checkout of this repo:

```bash
scripts/restore-drill.sh <node> /path/to/node.env
```

The script reads `LITESTREAM_IMAGE` and `WEB_IMAGE` from your shell, not from
`node.env`. If `node.env` sets registry tags for them, export both first, or the
script uses the local `:dev` images:

```bash
export LITESTREAM_IMAGE=<the value in node.env> WEB_IMAGE=<the value in node.env>
```

The script ([restore-drill.sh](../scripts/restore-drill.sh)):

1. restores the node's newest backup from S3 into a scratch Docker volume, with a
   full integrity check, and never touches the running node;
2. runs `library-web check-db` on the copy;
3. prints the copy's row counts next to the live node's counts;
4. removes the scratch volume.

What decides the exit code:

- By default it exits non-zero only if the **copy** fails `check-db`. If the live
  check fails, it prints a warning and the drill can still pass.
- `--strict` also fails when any count differs, or when the live node cannot be
  checked.
- In production the live counts keep moving, so strict mode reports writes made
  since the last upload. Use `--strict` only on an idle node. The end-to-end test uses
  it after waiting for the upload ([e2e.sh](../scripts/e2e.sh#L125)).

**Check it worked**

- The output ends with `DRILL PASSED`.
- The copy's counts are close to the live counts. A difference of the last second's
  writes is normal.

## Check a database for corruption

**When:** you suspect a bad disk, a crash, or an odd restore.

**Steps**

1. On a running node:

   ```bash
   docker compose -p <node> -f node.yml --env-file node.env exec web library-web check-db
   ```

   It runs SQLite's `integrity_check` and `foreign_key_check`, and prints the row
   counts of books, members, loans and users. It never creates or changes the
   database ([check_db](../apps/web/src/commands.rs#L121)). It exits non-zero when a
   check fails.
2. If it fails, stop the node and restore from S3 (see
   [Restore to a point in time](#restore-to-a-point-in-time), without `-timestamp`
   to take the newest backup). How much you lose depends on when the damage began.

**Check it worked**

- The output has `integrity: ok` and `foreign keys: ok`.

## Background jobs

For the Postgres deployment ([docs/JOBS.md](JOBS.md)); the SQLite nodes have no jobs.

Every `jobs` command below runs in the worker's container, with its environment:
`docker compose exec worker library-web jobs <command>`. Nothing here needs the API to stop:
borrowing and returning keep working while jobs wait.

| Alert | Section |
|---|---|
| `JobsQueueStalled`, `JobsNoLeader`, `JobsWorkerDown` | [Jobs: a queue is not moving](#jobs-a-queue-is-not-moving) |
| `JobsFailing` | [Jobs: failed jobs](#jobs-failed-jobs) |
| `JobsLeaseLost` | [Jobs: a lease was lost](#jobs-a-lease-was-lost) |

`JobsWorkerDown` and `JobsLeaseLost` need the `jobs-worker` scrape job in
`deploy/monitoring/prometheus.yml` (commented out there until a worker exists to scrape).

## Jobs: a queue is not moving

**When:** `JobsQueueStalled` (the oldest ready job in a queue is over 5 minutes old, for 10
minutes), `JobsNoLeader`, `JobsWorkerDown` (a worker's metrics scrape fails for 2 minutes), or
members report missing emails.

**Steps**

1. `jobs status`. Read the row of the queue and the warning lines under it.
2. If the queue shows `yes` in the `paused` column, someone paused it; the reason is in
   `SELECT * FROM job_queue_pauses`. If the reason no longer holds: `jobs resume <queue>`.
3. If a warning lists kinds that no live worker can run, the worker that runs them is missing
   or lacks its settings: `JOBS_SMTP_URL` for `loan_email.v1`, `JOBS_WEBHOOK_ENDPOINTS` for
   `webhook.v1`. Fix the worker's environment and recreate it with `up -d`.
4. If `jobs processes` shows no worker, or only `STALE` ones, start the worker:
   `docker compose up -d worker`, then read `docker compose logs worker` for the reason it
   stopped (a worker that logs "fenced itself off" was suspended or cut off for over 3 minutes;
   restarting it is the fix).
5. If `running` equals the queue's slots and stays there, handlers are slow or stuck:
   `jobs list --state running --queue <queue>`, then `jobs show <id>` for their progress and
   attempt. A stuck handler is stopped by its deadline (15 minutes by default, 2 for emails, 1 for
   webhooks). To stop one sooner: `jobs cancel <id>`. If the queue is simply too busy, raise its slots in
   `JOBS_QUEUES` or run a second worker.
6. `JobsNoLeader` alone: a leader election needs a worker and the database. Check that a worker
   runs and that its `/readyz` answers 200. Without a leader nothing reaps crashed jobs or deletes
   old ones, but running jobs still finish. If every worker is healthy and still none is leader,
   a dead worker's database session may still hold the leader lock (Postgres frees it only when
   it notices the client is gone, which takes up to about 30 s after a host dies). Find the holder, in `psql` against
   the application database (the key `0x6A6F_6273_6C65_6164` is 1785684595 in `classid` and
   1818583396 in `objid`):

   ```sql
   SELECT l.pid, a.client_addr, a.backend_start, a.state, a.state_change
   FROM pg_locks l JOIN pg_stat_activity a USING (pid)
   WHERE l.locktype = 'advisory' AND l.classid = 1785684595
     AND l.objid = 1818583396 AND l.objsubid = 1;
   ```

   If its `client_addr` is a worker that is gone (or one that is not leader), end the session:
   `SELECT pg_terminate_backend(<pid>);`. A live worker takes the lock within 10 s. Never end
   the session of the worker that exports `jobs_leader 1`.

**Check it worked**

- `jobs status` shows the oldest wait falling, and `jobs_oldest_available_seconds` drops below
  60 s.
- Exactly one worker exports `jobs_leader 1`.

## Jobs: failed jobs

**When:** `JobsFailing`, or `jobs status` shows failures in the last 24 h.

A job fails when its attempts run out (10, over at most about 43 minutes, longer when a receiver
sends `Retry-After`), when its handler reports a permanent error, or when its worker died twice
while running it.

**Steps**

1. `jobs list --state failed` and `jobs show <id>` for a few of them. The last entry of `errors`
   says why: `cause` is `error`, `permanent`, `bad_args`, `deadline_exceeded`, `panic`,
   `lease_expired` or `process_lost`. (Earlier entries can also be `cancelled` or
   `released_on_shutdown`, which never fail a job.)
2. By cause:
   - **SMTP or a receiver was down** (`error`, connection or timeout messages): fix or wait for
     the remote side, then replay what failed during the outage:
     `jobs retry --failed --kind loan_email.v1 --since 6h`. A resent email has the same
     `Message-ID`, and a resent webhook the same `webhook-id`, so receivers can drop duplicates.
     A job whose unique key is held by another waiting or running job is skipped; the command
     says so for a single id.
   - **`permanent`** (an invalid address, a 404 from a receiver): retrying gives the same answer.
     Fix the data or the endpoint first, or leave it.
   - **`bad_args`**: the job's arguments no longer match its kind. A deploy changed the job's
     shape without a new version. Fix the code; these jobs cannot be repaired by retrying.
   - **`lease_expired` / `process_lost` twice** (a poison job): it killed or hung its worker.
     Leave it failed, read the worker's logs around its `finished_at`, fix the handler, then
     `jobs retry <id>`.
3. For one endpoint that keeps failing while you talk to its owner: `jobs pause webhooks
   --reason "receiver X down, ticket 123"`. This pauses all webhooks; resume as soon as you can.

**Check it worked**

- The retried jobs reach `succeeded` (`jobs list --kind <kind> --limit 20`).
- `jobs_failed_recent` stops growing.

## Jobs: a lease was lost

**When:** `JobsLeaseLost`: in the last 15 minutes a worker cancelled a handler because it could
not renew the job's lease (`jobs_lease_lost_total`), or found its outcome write refused because
the job had been taken over (`jobs_fence_rejected_total`). The job may have run twice, once
under each worker; emails and webhooks carry stable ids, so receivers can drop the repeat.

**Steps**

1. `docker compose logs worker --since 30m`, and look for `jobs.lease_lost`,
   `jobs.lease_expired` and `jobs.fence_rejected` (they name the job), and for
   `jobs: heartbeat failed` or `timed out` just before them. A heartbeat that fails means the
   worker could not reach Postgres, or Postgres was too slow to answer within one heartbeat
   (20 s): check the database ([Postgres down](#postgres-down)) and `jobs_db_errors_total{op="heartbeat"}`.
2. A worker whose host froze or was starved of CPU for longer than a lease (60 s by default)
   shows the same signs, with `process_lost` in `jobs show <id>` for the jobs it held. Look at
   the host, then at the memory limit of the worker's container.
3. `jobs show <id>` of a job named in the logs: the last `errors` entries say how its attempts
   ended. Nothing needs repairing: the job was retried or finished by the other worker.
4. If it keeps happening on one worker, restart it (`docker compose up -d worker`).

**Check it worked**

- `increase(jobs_lease_lost_total[15m])` and `increase(jobs_fence_rejected_total[15m])` are 0 and
  the alert clears.

## Jobs: add a webhook endpoint

**When:** a new receiver must get loan events.

**Steps**

1. Generate a secret: `echo "whsec_$(openssl rand -base64 32)"`. Give it to the receiver's owner
   over a secure channel.
2. Append the URL to `JOBS_WEBHOOK_ENDPOINTS` (comma-separated) and set
   `JOBS_WEBHOOK_SECRET_<n>`, where `<n>` is its position counting from 1. Never reorder the
   existing entries: secrets are matched by position. Do it in the environment of **both** `web`
   and `worker`.
3. `docker compose up -d web worker`.

Removing an endpoint makes its waiting jobs fail as `permanent` ("no longer configured").

**Check it worked**

- Borrow a book. `jobs list --kind webhook.v1 --limit 5` shows one job per endpoint, and the new
  one reaches `succeeded`.

## Production to-dos

The example leaves these out. Do them before real use.

- TLS in front of the web app, and `COOKIE_SECURE=true`.
- A secrets manager for `POSTGRES_URL` and the S3 keys. The committed local keys are
  for the local cluster only.
- Separate Postgres roles for migrations and for the runtime.
- Host and disk metrics, with alerts, so a filling disk is seen early.
- Alertmanager, so alerts reach a person. Today they are only visible at
  `http://127.0.0.1:9090/alerts`.
- S3 bucket versioning and lifecycle rules.
- A fixed rule that maps each branch to its node (for example one subdomain per
  node), so users are sticky.

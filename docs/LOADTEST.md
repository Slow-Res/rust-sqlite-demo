# Load test

How much load can one node handle on a 2 CPU / 4 GB budget? This test answers that
with two numbers: the most simultaneous users, and the most requests per second, that
still meet the pass rule below. `scripts/loadtest.sh` builds a separate cluster, seeds
it, runs k6 against it, and searches for the highest passing level.

## Modes

`LT_MODE` picks what runs under the limit:

- **`web`** (the default) measures one box: nginx in front of the Rust web app, with SQLite
  inside the app (or Postgres in the same box, see `LT_DB`). Project `lt-web`, every
  container on cores `NODE_CPUS`. There is no Litestream, sync worker, restore gate,
  Garage or Prometheus, so sync, Litestream and drain fields read `n/a`. Sampling is
  `docker stats` of all box containers and k6 in one call.
- **`node`** adds Litestream and the sync worker on the same 2 cores (the limits and the
  topology below), with Postgres, S3 (Garage) and Prometheus outside the limit. No nginx
  here: k6 aims at web directly, as before.

### nginx in front (web mode)

k6 talks to nginx (`http://nginx:80`), and nginx proxies to web over keep-alive
connections. That is a real deployment shape (nginx, app, database on one machine), and
the shape of the video this test is compared with. nginx shares the 2 cores with the app
and takes its slice of the memory. Its config is `deploy/compose/local/loadtest-nginx.conf`
(2 workers, gzip level 1 for JSON, the access log buffered to a file inside the container,
not to docker logs, so dockerd does no logging work outside the box). Seeding goes through
nginx too. The runner counts the access-log lines before and after each level and prints
the delta (`nginx_access_log` in `level.json`), which proves the traffic went through
nginx; nginx's own health check is not logged.

### `LT_DB`: SQLite or Postgres (web mode only)

`LT_DB=sqlite` (the default) or `LT_DB=postgres`. `LT_DB=postgres` with `LT_MODE=node`
exits with a usage error. With Postgres, `loadtest.web-pg.yml` is layered on
`loadtest.web.yml` and adds a `postgres` container (`postgres:18.6`, data in a named volume
on disk, because the SQLite file is on disk too) to the same box, and points web at it
with `DATABASE_URL`. The project is still `lt-web`, so one `down` removes either variant.
The app image must have been built with the `postgres` feature: `make loadtest-image`
builds `sqlite-production/web:lt`, the image web mode uses (`WEB_IMAGE` overrides it), and
`up` dies with "run make loadtest-image" when it is missing.

The memory is split per container, so each variant adds up to exactly 4096 MiB and `up`
checks that sum. Limits are per container because a shared pool would let one process
starve the others invisibly; this way a container that runs out is OOM-killed and named.

| Variant | nginx | web | postgres | total |
|---|---|---|---|---|
| sqlite | 256m | 3840m | | 4096 MiB |
| postgres | 256m | 1024m | 2816m | 4096 MiB |

Why this split: in the SQLite box the database is a file inside the web container, so that
container holds the app *and* the SQLite file's OS page cache, and gets nearly everything
(nginx needs little). In the Postgres box the database has its own container, which holds
`shared_buffers` and the page cache of Postgres' files, so it gets the most, and the Rust
app (a few hundred MiB, more with many sessions) keeps 1 GiB. The split is configurable: `NGINX_MEM`,
`WEB_MEM` and `PG_MEM` (for example `PG_MEM=2048m WEB_MEM=1792m`, a number with `m` or `g`).
The rule does not change: the box total must be 4096 MiB, and `up` dies with a message
otherwise. `scripts/loadtest.sh config` prints the rendered compose file of the box
(`docker compose config`), to see the limits and Postgres flags without starting anything.

Postgres runs with defaults except for memory, planner and WAL settings. Memory:
`shared_buffers` = 25% of `PG_MEM` and `effective_cache_size` = 75% of it, rounded to MB
(704MB and 2112MB at 2816m; the runner derives them from `PG_MEM` and passes them as
flags), `max_connections=50` (raised to `DB_POOL` + 20 when the pool is above 30), `work_mem=4MB`
(`PG_WORK_MEM`), `maintenance_work_mem=176MB` (and `shm_size` 256m). Planner and WAL, the
standard SSD values as PGTune gives for a 2-CPU web workload:
`random_page_cost=1.1`, `effective_io_concurrency=200`, `min_wal_size=1GB`,
`max_wal_size=4GB`, `max_parallel_workers_per_gather=1`. With the hard-disk default
`random_page_cost=4` the planner chose a Seq Scan of 52k rows (23 ms) for a whole-title
book search, an `ILIKE` OR over three trigram GIN indexes; with 1.1 it uses the indexes
(6 ms). Autovacuum runs more often (`autovacuum_vacuum_scale_factor=0.05`,
`autovacuum_vacuum_insert_scale_factor=0.05`), so the visibility map keeps index-only scans
cheap under constant updates; without it deep book-list pages did about 23k heap fetches
per page. `synchronous_commit=on` and `fsync=on`
stay, so a Postgres commit is durable on power loss. That is stronger than SQLite's
`synchronous=NORMAL` (WAL), which can lose the last commits on power loss. Weigh this when
you compare the two variants: Postgres pays for a durability SQLite does not give here.
The credentials (`library` / `library`) are local only.

In web mode `level.json` and the level line show the box CPU (the sum over the box
containers, out of 200%) and its split, for example
`cpu box avg 172% max 199% (web 118, postgres 47, nginx 9)`, and the box memory peak
(the highest per-sample sum) with each container's peak, for example
`mem box peak 1.9 GiB (web 1.1, postgres 0.7, nginx 0.02)`. Restart and OOM checks cover
every box container. The results directory is named `<UTC timestamp>-<db>`, and
`summary.json` carries `db`.

Note for `web`: nothing prunes `change_log`, so it grows by one row per synced write during
a run on SQLite. This is a small extra cost, and real nodes do not have it once the worker
runs. The compose files are `deploy/compose/local/loadtest.web.yml` (standalone: no restore
service is needed, because a new named volume takes over the ownership of `/data` (the
app user, uid 10001) from the image) and `loadtest.web-pg.yml`. Nothing is published on the
host; the runner reaches web through the container's address, and k6 joins
`lt-web_default`. `scripts/loadtest.sh down` removes the projects of both modes.

The sections below describe `node` mode unless they say otherwise; the workload, data,
error rule and pass rule are the same in both.

## What it measures, and why

A node is one VM: the web app, Litestream and the sync worker share the VM's CPUs and
memory. A load test of the web app alone would miss the other two. They compete for the
same cores, and Litestream and the worker read the same SQLite file the web app writes.
So the limits apply to the whole node, and the test watches all three containers.

There are two load models, because they answer different questions:

- **Users.** N virtual users, each a person at a desk: log in, 20-40 actions with 1-5 s
  of think time after each, log out, start over. This is the number to quote ("the node
  serves about N staff at once"). The request rate it produces follows from N.
- **Requests per second.** R actions per second, no think time, whatever the number of
  users. This finds the raw throughput limit. It uses k6's `ramping-arrival-rate`, which
  keeps the rate fixed even when responses slow down, and counts the iterations it could
  not start (dropped iterations) as a sign of saturation. This model measures
  steady-state actions **without logins**: `setup()` logs in `SESSION_POOL` sessions
  (default 50) one after the other, before the scenario starts, so there is never more
  than one Argon2 hash at a time, and each VU uses session `(VU - 1) mod pool` by setting
  its cookie. VUs never call login, `me` or logout during the run, and the setup logins
  are not part of any measured request. The users model, in contrast, includes the logins
  (and `me`, logout) in its numbers. Why: a burst of logins can OOM-kill the web
  container (see the caveats). That is an app finding, reported separately; the harness
  must not turn it into the requests-per-second number.

## Topology and limits

`scripts/loadtest.sh up` starts two compose projects, like `scripts/e2e.sh` does with
`e2e-`:

| Project | Services | Cores | Port |
|---|---|---|---|
| `lt-central` | Postgres, Garage, Prometheus | `CENTRAL_CPUS`, default 8-13 | Postgres 5436, Prometheus 9092 |
| `lt-node1` | web, Litestream, sync-worker (and restore, a one-shot) | `NODE_CPUS`, default 6,7 | web 127.0.0.1:4201 |

The shared network is `lt-sqlite-prod`. k6 runs as a container (`grafana/k6:1.3.0`, a
pinned version) on cores `K6_CPUS`, default 0-3, attached to `lt-node1_default` and
aimed at `http://web:4000`, so no docker-proxy sits in the path. Set `BASE_URL` to aim
it elsewhere.

The node gets these limits from `deploy/compose/local/loadtest.node.yml`:

| Service | Memory (no swap) | CPUs |
|---|---|---|
| web | 3g | |
| litestream | 512m | |
| sync-worker | 512m | |
| restore (one-shot, finished before load) | 512m | |

All four use `cpuset: 6,7`, so the node as a whole is capped at 2 CPUs. The memory limits
of the three running services add up to 4 GB. `memswap_limit` equals `mem_limit`, so there
is no swap, like on a VM. The central services are pinned by
`deploy/compose/local/loadtest.central.yml`. `up` prints the proof: `docker inspect` of
every node container shows the cpuset and memory limits, and `nproc` inside the web
container prints 2.

### The core choice

The host has 16 cores without hyperthreading, of three kinds: 0-5 are 5.1 GHz performance
cores, 6-13 are 4.4 GHz efficiency cores, and 14-15 are 2.5 GHz low-power cores.

- **The node runs on 6 and 7.** A cloud vCPU is slower than a 5.1 GHz performance core, and
  4.4 GHz is closer. Two cores also leave the best cores to the load generator.
- **Cores 14 and 15 are never used.** At 2.5 GHz they would make the result a statement
  about those cores. The script refuses them.
- **k6 runs on 0-3.** The load generator must not be the bottleneck, and it must not share
  cores with the node.
- **Postgres, Garage and Prometheus run on 8-13.** The sync worker talks to Postgres, so
  its cost belongs to the node, but Postgres itself would not run on the node's VM.

The three groups never overlap (the script checks).

## Data

`up` seeds once, through the HTTP API, so the change-log triggers, Litestream and the sync
worker see real writes. Defaults: `BOOKS=50000`, `MEMBERS=10000`, `LOANS=20000`
(override them in the environment; `LOANS` must not exceed `BOOKS`). Books have 1-5
copies and an ISBN-13 (four in five), members have unique emails. Loan *j* borrows book
*j+1* for member *j mod MEMBERS*. It stays active when *j* is a multiple of 10 and it is
among the first two loans of its member (*j < 2 x MEMBERS*); every other loan is returned
straight away. So the history is mostly returned loans and about 10% (2000) stay active,
at most two per member, far below the limit of 5 per member and the copies of every book. Staff login
is `admin` with the local-only test password `loadtest-local-only`, created with
`library-web create-user admin --password-stdin` like in the e2e test.

Before any measurement, `up` waits until the sync worker has shipped everything
(`sync_pending_changes` is 0 after a cycle that started after the last write) and until
Litestream has uploaded it (its position, `litestream_txid`, and its upload counter have
stopped moving). It also checks that Postgres holds as many rows as the API reports.

The data is never reset between levels, so the database grows during a run. The
summaries state the row counts at the start and the end.

## The action mix

A user action is an intent. `MIX` (see "Overrides") picks the table. `MIX=nosearch`, the
default, has no book search: book search sorts every match by title and counts them
exactly, so it grows with the data and was about 80% of the Postgres time. Its 15% goes
to one more read, browsing the available books, and one more write, deleting a book.
Each action is picked by these shares:

| Kind | Action | Share |
|---|---|---|
| read | browse books: the next page, or page 1 (see "Browsing and paging") | 20% |
| read | browse available books (`GET /api/books?availability=available`, same next-page browsing, its own cursor) | 11% |
| read | open a random book | 10% |
| read | list members (next page or page 1), or open a member | 8% |
| read | list loans (active, returned or all; next page or page 1) | 7% |
| write | borrow a book | 13% |
| write | return a loan | 13% |
| write | edit a book | 5% |
| write | add a book | 4% |
| write | delete a book | 4% |
| write | add a member | 3% |
| write | edit a member | 2% |

`MIX=search` restores the earlier mix exactly, so earlier runs stay reproducible: no
browse available and no delete book, and instead search books (a word from seeded
titles/authors, or one whole seeded title) at 15%. In that mix 60% of the actions are
reads and 40% writes.

In the default mix 56% of the actions are reads and 44% writes. An edit first reads the record
(`edit_book_get`, `edit_member_get`), a supporting request. A return only ever returns a
loan from the user's **own stack** of loans it borrowed itself; it never takes loans from
other users, so users do not fight over loans and the number of active loans stays
steady. If the stack is empty, the return becomes a borrow and is counted as
`return_converted_to_borrow`; the summary reports the count and its share of the return
actions per level (in the rps model, with many short-lived VUs, the share is larger). The
summary also prints each level's starting `active_loans`, and the real split of the
requests next to the intended split of the actions. The edits keep the copies, ISBN and email, so they
cannot clash with other data (the API has no version column; a PUT replaces the record).

A delete only ever deletes a book from the user's **own list** of books it added itself
(`add_book` remembers the id of every 201). Add (4%) and delete (4%) balance, so the books
table keeps about the same size during a ramp; in the earlier runs it grew from 50,000 to
144,000 books and every step had more data than the one before. If the list is empty, the
delete becomes an add and is counted as `delete_converted_to_add` (the summary prints
it per level when the mix has deletes). A delete answers 204, or 409 if another user has
borrowed the book in the meantime (`book_has_active_loans`); the book then stays, and
the id is dropped from the list. 404 is an error.

### Browsing and paging

`PAGING=cursor` (the default) makes browsing look like a user paging through a list.
For `browse_books`, `browse_available`, `list_members` and `list_loans` (one cursor per status) each VU keeps
the `next_cursor` of its last answer for that list. With probability 0.75, if it has a
cursor, it asks for the next page (`?after=<cursor>`); otherwise it asks for page 1, which
resets the cursor (and so does the last page, whose `next_cursor` is null). Every page
costs about the same as page 1, because the API pages by keyset (see
`docs/ARCHITECTURE.md`, "List paging").

`PAGING=offset` keeps the earlier workload: a random `page=N` of the whole list, so
`OFFSET` reaches about 50,000 rows deep. Use it to reproduce earlier runs; their numbers
are not comparable with cursor runs, because a random deep page is far more expensive,
above all on Postgres. The summary header names the paging (`summary.json` has `paging`;
older summaries have none and were run with offset). The action mix and its write/read
split are the same in both.

Every request is tagged `action:<name>`, `kind:read|write|auth` and
`phase:ramp|hold|post`. Login, `GET /api/auth/me` and logout are `kind:auth` and count
toward the overall percentiles.

Users keep their own active loan ids across sessions. In the requests-per-second model
each VU logs in on its first iteration and keeps its cookie.

## The error rule

A request is an error if it is a network error or timeout (10 s), any 5xx, or any status
the action does not expect. Each action has an explicit list (`EXPECT` in
`loadtest/user.js`):

| Action | Expected |
|---|---|
| login, me | 200 |
| logout | 204 |
| browse_books, browse_available, search_books, open_book, list_members, open_member, list_loans | 200 |
| borrow | 201, or 409 (no copies left, or the member is at the 5-loan limit) |
| return | 200, or 409 (already returned; not expected, since only the user's own loans are returned) |
| edit_book_get, edit_member_get | 200 |
| edit_book, edit_member | 200 |
| add_book | 201, or 409 (duplicate ISBN) |
| delete_book | 204, or 409 (another user borrowed the book); 404 is an error |
| add_member | 201 |

A `Rate` metric counts the errors and `responseCallback` makes k6's own `http_req_failed`
agree. A counter `unexpected_status` records every unexpected answer by action and status
(0 means a network error or timeout); the level line and `summary.md` list them per
action and status, with an "other" row for statuses the counter does not name. The summary shows the share of each expected non-2xx outcome per action. If one is
above 5%, the summary says so at the top: the mix is then not measuring what it claims
(for example, if most returns hit 409, they are not doing the work of a return).

## Step ramp (`ramp`)

`scripts/loadtest.sh ramp [START] [STEP] [STEP_S]` (web mode only; defaults 100, 100, 60)
answers a different question from `search`: starting at START users, add STEP users every
STEP_S seconds, and keep going until one step fails the pass rule or `MAX_USERS` (5000) is
reached. It is one k6 run (a `ramping-vus` scenario of stairs, in `loadtest/user.js` with
`MODE=ramp`), not one run per level, so it needs no pause or drain between steps. All
arguments are positive integers and STEP_S is at least 20. Step *k* has START + k*STEP
users: a 10 s ramp up by STEP, then a hold for the rest of the step.

**The workload is the same** (the same mix, think times and data as `users`) with one
difference: **each user logs in once, when it starts, and keeps the session for the whole
run.** There is no logout and no re-login between sessions; `me`, `login` and `logout`
leave the steady loop, and the mix of the actions (`MIX`) is unchanged. This is
how real users behave (one login a day), and it stops the simulated users from re-running
Argon2 about every 90 s, which spends CPU that real traffic would not. New users still log
in as they arrive: about STEP per step, inside the 10 s ramp (that is why a step's p95 is
a bit higher than the hold alone would give). If a request returns 401 (the session was
lost) the user logs in again; the number of re-logins per step is reported.

**Per-step numbers.** Every request is tagged `step` (elapsed time since the scenario
started divided by STEP_S, taken when the request starts). Step *k*'s p50/p95/p99, error %
and req/s cover the requests that started in that step only, ramp-up included, not the
whole run. The pass rule is the same as below with **errors < 2%** in this mode (`ERR_PCT`
overrides it, 2 by default only here); restarts and OOM kills are recorded per step but are
not part of the ramp's rule.

**Warm-up (`WARMUP_S`).** A ramp that starts high (START 3000) would log in 3,000 users in
10 s, which is a burst of Argon2 hashes on a few hashing permits, and step 0 would fail for
that and not for the steady load. `WARMUP_S=N` (an integer of at least 30; default 0 = none)
puts an unjudged warm-up before the stairs: the users sign in gradually, 0 to START over the
first N/2 s, and hold START until N s. Then the stairs run as above, step 0 = START (judged),
and every step's window is counted from the end of the warm-up. The warm-up's requests are
tagged `step:warmup`, belong to no step and are never PASS or FAIL; it never stops the ramp.
Its numbers (req/s, p95, error %, box CPU, logins started) are printed as one line before the
table, and `summary.md` shows it after `warm-up: N s (unjudged)` in the header and again above
the table; `warmup.json` in the results directory has the same numbers.

```
WARMUP_S=120 scripts/loadtest.sh ramp 3000 100 30   # 2 min warm-up, then 3000 users, +100 every 30 s
```

**Early stop.** Each step is read 10 s after it ended (so requests that took long have
finished) from k6's REST API (`--address`, reached through the k6 container's address on
the box network). The steps' sub-metrics are declared as thresholds that never abort. The
API has no p99, so the p99 threshold's `tainted` flag says whether the p99 is over its
limit. The first step that fails stops k6 with SIGTERM, about 10-12 s after that step's
end, and k6 still writes its summary. Requests in flight at the time of the live reading
are missing from it, so **the report comes from k6's end-of-test summary per step**: the
first failing step is the first whose final values fail, and the result is the step before
it. The partial step after the failing one is dropped. A step failing in the live reading
but not in the final values is reported as passing, with a note. One line per step is
printed as it is evaluated, in the format of the table. The last step of a run that reached
`MAX_USERS` is printed when k6 ends. These live lines use the live numbers (the p99 cell is
`< 1000` or `>= 1000` there); the table in `summary.md` has the final ones.

**Memory of the load generator.** k6 creates all VUs for the highest step before the test
starts, so a large `MAX_USERS` costs k6 start-up time and memory. Lower it for a quick run.

**Per-step sampling** uses the same 5 s tick: CPU per container and the box sum (out of
200%), peak memory per container, k6's CPU, restarts and OOM per box container, and the
**CPU frequency of each box core** (`/sys/devices/system/cpu/cpuN/cpufreq/scaling_cur_freq`):
average and minimum per step. When two boxes run side by side, a frequency drop shows that
they slowed each other through the laptop's shared power budget. Short steps have few samples.

**Output**, in `loadtest/results/<timestamp>-<db>-ramp[-<prefix>]/`: `steps.jsonl` (one
object per step; `warmup.json` too with `WARMUP_S`), `summary.md` (a header naming the stack, the box, the memory split and the
rule, with "each user logs in once and keeps the session", then one row per step: users,
req/s, p50/p95/p99, error %, box CPU with its per-container split, peak memory per
container, k6 CPU, average / minimum frequency in MHz, clock (see "Reproducible runs"),
restarts / OOM, re-logins and PASS or FAIL, then the result line), `live-steps.jsonl` (what was read live), `k6-summary.json`,
`samples.jsonl`, `events.jsonl`, `k6.log`. The result line looks like

```
ramp result: highest passing step 1300 users (≈ 495 req/s); first failing step 1400 users (p95 612 ms / p99 1034 ms / err 0.5%) (clock: ok)
```

It exits 0 when at least one step passed (also when `MAX_USERS` was reached without a fail),
and 1 with "no passing step" when step 0 already fails. The two boxes of "Two boxes in
parallel" can each run a ramp at the same time:

```
PREFIX=lt- NODE_CPUS=6,7 K6_CPUS=0-2 scripts/loadtest.sh ramp
PREFIX=lt-pg- LT_DB=postgres NODE_CPUS=10,11 K6_CPUS=3-5 scripts/loadtest.sh ramp
```

## The pass rule

A level passes only if all of these hold over its **hold** (the ramp is excluded):

- p95 of all requests together < 500 ms (`P95_MS`)
- p99 of all requests together < 1000 ms (`P99_MS`)
- error rate < 1% (`ERR_PCT`)
- no node container restarted or was OOM-killed during the level
- requests per second only: dropped iterations < 1% of the scheduled ones (`DROP_PCT`),
  counted over the whole level, because k6 does not tag them by phase

The percentiles come from `phase:hold` sub-metrics of k6, so they are percentiles of the
hold window only. Latency is k6's `http_req_duration`: it never includes think time.

The headline numbers of a search give the measured request rate next to the level, for
example `highest passing level: 300 actions/s (≈ 325 req/s)` or `800 users (≈ 290
req/s)`, because one action is more than one request.

## How to run it

Needs Docker, `curl`, `jq` and `make images` (node mode) or `make loadtest-image` (web mode). Nothing outside the
`lt-` prefix is touched: not your `make up` cluster, not `e2e-`.

```bash
make images
scripts/loadtest.sh up                  # or: make loadtest-up
scripts/loadtest.sh users 100           # one level; exit 0 pass, 1 fail
scripts/loadtest.sh rps 200
scripts/loadtest.sh search users        # step up from 100 by x2, then bisect, then confirm
scripts/loadtest.sh search rps 50 2     # START and FACTOR are optional
scripts/loadtest.sh ramp 100 100 60     # +100 users a minute until a step fails (START STEP STEP_S)
scripts/loadtest.sh down                # or: make loadtest-down
make loadtest                           # up, search users, search rps, down
```

The search steps up from START by FACTOR until a level fails, bisects between the last
pass and the first fail until the gap is at most 10% of the pass, then runs the best
pass once more to confirm it. If that fails, it steps down 10% (rounded up, always at least one lower, never
below 1) and confirms again, at most twice. If the first level already fails, it steps down
by FACTOR until one passes. If level 1 fails, the result is "no passing level". START must
be a positive integer and FACTOR a number greater than 1.

Environment overrides (defaults in brackets):

| Variable | Meaning |
|---|---|
| `RAMP` (60), `HOLD` (300) | seconds of ramp-up and of measured hold |
| `P95_MS` (500), `P99_MS` (1000), `ERR_PCT` (1; 2 for `ramp`) | pass-rule limits; make them impossible to see the fail path |
| `MAX_USERS` (5000) | `ramp` stops here if no step failed |
| `WARMUP_S` (0) | `ramp`: seconds of unjudged warm-up before step 0 (0 to START over the first half, then a hold), 0 or an integer of at least 30. Example: `WARMUP_S=120 scripts/loadtest.sh ramp 3000 100 30`. See "Step ramp" |
| `NGINX_MEM` (256m), `WEB_MEM` (3840m; 1024m with Postgres), `PG_MEM` (2816m) | memory split of the box; must add up to 4096 MiB. See "`LT_DB`" |
| `DB_POOL` (empty: the app's 10) | the web app's database pool size, passed as `DATABASE_MAX_CONNECTIONS` (both backends); web mode only. A positive integer. With Postgres, `max_connections` stays 50 up to a pool of 30 and becomes `DB_POOL` + 20 above that. The summary header and `summary.json` (`db_pool`) record it; empty is recorded as the default (10) |
| `PG_WORK_MEM` (4MB) | Postgres' `work_mem`, an integer with a unit `kB`, `MB` or `GB`; applies with `LT_DB=postgres`. Recorded in the summary header and `summary.json` (`pg_work_mem`). Each query step of each connection may use it, so a larger pool multiplies it; stay inside `PG_MEM` |
| `DROP_PCT` (1) | dropped iterations limit, rps only |
| `BOOKS` (50000), `MEMBERS` (10000), `LOANS` (20000) | size of the seed, for `up` |
| `SEED_VUS` (16) | k6 VUs that seed |
| `SESSION_POOL` (50) | sessions logged in at setup in the rps model |
| `PAGING` (`cursor`) | `cursor`: "next page" browsing with `after`; `offset`: random deep `page=N`. See "Browsing and paging" |
| `MIX` (`nosearch`) | `nosearch`: no book search, browse available (11%) and delete book (4%) instead; `search`: the earlier mix with `search_books` (15%). See "The action mix". The summary header names it (`summary.json` has `mix`; older summaries have none and were run with `search`) |
| `NODE_CPUS` (6,7), `CENTRAL_CPUS` (8-13), `K6_CPUS` (0-3) | core sets; they may not overlap or include 14-15 |
| `PREFIX` (`lt-`) | name prefix of every resource of one box (compose projects, containers, networks, volumes, `loadtest/results/.state-<PREFIX>`); must start with `lt-` and end with `-`, e.g. `lt-pg-`. `down` removes only that prefix. See "Two boxes in parallel" |
| `LT_MODE` (`web`) | `web` or `node`, see Modes |
| `LT_DB` (`sqlite`) | `sqlite` or `postgres`, web mode only |
| `WEB_IMAGE` (`sqlite-production/web:lt`) | the web image of web mode (build it with `make loadtest-image`) |
| `K6_IMAGE` (`grafana/k6:1.3.0`), `BASE_URL` (`http://nginx:80` in web mode, `http://web:4000` in node mode) | the k6 image, and where k6 aims, as seen from its container |
| `PAUSE_S` (30) | pause between levels of a search, after the drain |
| `MAX_LEVEL` (100000) | the search gives up above this level |
| `SAMPLE_S` (5) | sampling tick during a level |
| `COOL_C` (55), `COOL_MAX_S` (900) | cool-down gate: a run (`users`, `rps`, `search`, `ramp`) starts its first load only when the CPU package is below `COOL_C` degrees C (the `x86_pkg_temp` thermal zone, else the hottest zone), checked every 5 s and printed about every 30 s. After `COOL_MAX_S` seconds it warns and starts anyway; the summary records that it timed out. `COOL_C=0` turns the gate off. See "Reproducible runs" |
| `DRAIN_TIMEOUT_S` (900) | longest wait for the sync worker to drain |
| `RESULTS_DIR` | results directory (default `loadtest/results/<UTC timestamp>`, with `-sqlite` or `-postgres` appended in web mode, and `-<prefix>` when `PREFIX` is not `lt-`) |
| `LT_ADMIN_PASSWORD` (`loadtest-local-only`) | password of the local-only `admin` account |

### Two boxes in parallel

Web mode can run two boxes at once, for example SQLite and Postgres, on different cores.
Give each its own `PREFIX`, `NODE_CPUS` and `K6_CPUS`:

```
PREFIX=lt- LT_DB=sqlite NODE_CPUS=6,7 K6_CPUS=0-2 scripts/loadtest.sh up
PREFIX=lt-pg- LT_DB=postgres NODE_CPUS=10,11 K6_CPUS=3-5 scripts/loadtest.sh up
```

Pass the same `PREFIX` to every later command (`users`, `rps`, `search`, `ramp`, `down`) of that box.
Cores 6-9 and 10-13 are separate E-core clusters with their own L2, so the boxes do not
share a cache. They do share the laptop's power budget, so absolute numbers can be a bit
lower than in a solo run, but the comparison between the two boxes stays fair. Node mode
publishes fixed host ports, so it supports one box at a time.

A harness failure (a query or jq program fails, k6 dies, Postgres cannot be read) ends the
script with exit 2 and a message. It is never reported as a failed level. Whatever was
measured up to then is written to `summary.md` and `summary.json`, marked
`ABORTED: <reason>`, also after Ctrl-C.

Between levels the runner waits until the sync worker's pending changes are 0 (and
reports how long that took), then pauses 30 s.

During every level it samples on a fixed 5 s tick (`SAMPLE_S`): `docker stats` of the three
node containers (CPU summed, out of 200%, and memory) and of the k6 container (CPU, out of
400% for its 4 cores), plus the worker's pending changes and lag and Litestream's
position and PUT counter, from Prometheus. The queries run at the same time, so a slow
`docker stats` does not stretch the tick. Peaks (CPU, memory, pending, lag) are the
maximum of these samples, so a spike shorter than the tick can be missed. A warning is
printed when k6 used more than 80% of its cores: the load generator may then be the
bottleneck. After each level it checks `RestartCount`
and `OOMKilled`. Each sample also holds the current frequency of every box core (kHz, from
`scaling_cur_freq`), the cumulative throttle counters of the box cores and the package
(`core_throttle_count`, `package_throttle_count`), the package temperature and the restart counts
of the box containers; ramp mode reports them per step.

### Reproducible runs

The laptop's CPU package has a sustained power limit (28 W, RAPL PL1). Under hours of load
the box cores slow down by themselves (0.7-1.6 GHz seen on cores 10 and 11 in the middle of a
ramp), so two runs of the same code can differ by more than the change being measured. Three
things make runs comparable:

- **Cool-down gate.** Before the first load the runner waits until the package is below
  `COOL_C` (55 C), for at most `COOL_MAX_S` (900 s). A run that timed out says so in its
  summary ("TIMED OUT ... the run started warm").
- **Lock the clocks.** Pin the box cores to one frequency, so the clock cannot follow the
  power budget. This needs root, once per boot, for each box core (here cores 10 and 11 at
  2.4 GHz):

  ```
  for n in 10 11; do echo 2400000 | sudo tee /sys/devices/system/cpu/cpu$n/cpufreq/scaling_{min,max}_freq; done
  ```

  The runner needs no root: at the start of a run it reads `scaling_min_freq` and
  `scaling_max_freq` of the cores in `NODE_CPUS` and records `cpu clock: locked 2400 MHz on
  cores 10,11` when min equals max on all of them, or `unlocked (min-max ...)` otherwise, in
  the summary header and in `summary.json` (`cpu_clock`, `cool_down`). An unlocked run prints a
  warning. Cap the other cores too, so they cannot take the power budget.
- **The `clock` column of a ramp.** Every step records the growth of the box cores'
  `core_throttle_count` (summed) and of the package's `package_throttle_count` over its window
  (`throttle {core, package}`), and the package temperature (`pkg_temp_c {avg, max}`), in
  `steps.jsonl`. `clock` says `ok` when the clock is locked, the step's average frequency is at least 95% of the
  lock and nothing throttled; `THROTTLED` when any throttle counter grew; `drift 2210` when the
  average (here 2210 MHz) is below 95% of the lock (both words when both apply); `-` when the run is
  unlocked and nothing throttled, so the clock is not known (`clock_ok` is `null` in
  `steps.jsonl`; `false` for the other two). The column is a flag only: it never changes
  PASS or FAIL. The `ramp result` line ends with `(clock: ok)` when the first failing step and the
  highest passing step were both `ok`, with `(clock: not verified)` when one is `-`, and with a
  warning naming the step when one of them was not steady: treat that result as a lower
  confidence one and repeat it.

`users`, `rps` and `search` record the lock and the cool-down in their summary but have no per-level clock
column; the raw counters and temperatures are in each level's `samples.jsonl`.

`scripts/loadtest.sh clockcheck` runs the lock check and the cool-down gate alone, and prints
the package temperature and the throttle counters, without touching any container.

### Results

Each run writes `loadtest/results/<UTC timestamp>/` (not committed): one directory per
level with `k6.log`, `k6-summary.json` (k6's own numbers), `samples.jsonl` (the raw
samples), `events.jsonl` (Docker's die and OOM events of the node containers) and `level.json`, plus `summary.json` and `summary.md`. The table has one row
per level: result and reason, requests per second, p50/p95/p99, error %, node CPU
(average and maximum, of 200%), peak memory per container, maximum sync pending and
lag, whether Litestream's PUT counter rose during the hold (and how many 15 s buckets were flat), the drain time and
dropped iterations. Second tables show p95/p99 per action, the expected non-2xx shares,
and the intended versus real read/write split.

## How long it takes

A level takes ramp + hold + about 30 s for the checks and the drain, so about 6.5 minutes
with the defaults, plus 30 s of pause between levels. A search needs roughly 8-12 levels,
so allow about 1 to 1.5 hours per search, and 2 to 3 hours for `make loadtest`. Seeding
the default data takes a few minutes (it is about 100,000 API calls).
`make loadtest-up` once, then single levels, is the way to explore.

## Caveats

- **The laptop is not a cloud VM.** The cores are real cores without hyperthreading,
  where a cloud vCPU is usually a hyperthread; the disk is a local NVMe SSD; the network is
  a Docker bridge on the same machine. The numbers are a good guide to the shape of the
  limits (what saturates first), and an optimistic one to their size.
- **Other host processes may share the cores.** The `cpuset` restricts the containers to
  cores 6 and 7, but nothing keeps other processes of the host off them, and the kernel
  and Docker work on them too. Check the load average of the host before a run. A noisy
  neighbour shows up as a fail with an unremarkable node CPU.
- **The database grows during a run.** Every level adds books, members, loans and
  sessions, and nothing is reset, so later levels run on a larger database than earlier
  ones. The summary states the row counts at the start and the end.
- **A burst of logins can OOM-kill the web container.** Every login hashes a password with
  Argon2 (about 19 MiB each), and the web app caps concurrent hashes at max(1, cores/2) permits
  (`PASSWORD_HASH_PERMITS` overrides it). Before that cap, an earlier version of the rps
  model logged in per VU and, once the node fell behind,
  started hundreds of VUs that all logged in; web reached its 3 GB and was killed. The rps
  model now logs in a fixed pool sequentially at setup. The users model still logs in
  during the run, as real users do, and a restart or OOM kill in any level is a failed
  level, not a harness error.
- **Web memory is not released after load**, so later levels of a run start at a higher
  RSS than earlier ones.
- **The harness's own tools** (`docker stats`, dockerd, jq, curl) are not pinned to cores
  and may run on cores 6 and 7.
- **A crash during the 30 s pause belongs to no level.** Restarts are counted from the
  start of a level (after the node is healthy) to its end.
- The load generator and the measuring (`docker stats`, Prometheus queries) take a little
  CPU on the host, outside the node's cores.

## Results

Not yet run.

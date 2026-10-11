# Load test

How much load can one node handle on a 2 CPU / 4 GB budget? This test answers that
with two numbers: the most simultaneous users, and the most requests per second, that
still meet the pass rule below. `scripts/loadtest.sh` builds a separate cluster, seeds
it, runs k6 against it, and searches for the highest passing level.

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
  not start (dropped iterations) as a sign of saturation.

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
*j+1* for member *j mod MEMBERS*, and nine in ten are returned straight away, so the
history is mostly returned loans and about 10% (2000) stay active, at most two per
member, far below the limit of 5 per member and the copies of every book. Staff login
is `admin` with the local-only test password `loadtest-local-only`, created with
`library-web create-user admin --password-stdin` like in the e2e test.

Before any measurement, `up` waits until the sync worker has shipped everything
(`sync_pending_changes` is 0 after a cycle that started after the last write) and until
Litestream has uploaded it (its position, `litestream_txid`, and its upload counter have
stopped moving). It also checks that Postgres holds as many rows as the API reports.

The data is never reset between levels, so the database grows during a run. The
summaries state the row counts at the start and the end.

## The action mix

A user action is an intent. Each is picked by these shares:

| Kind | Action | Share |
|---|---|---|
| read | browse a random page of books | 20% |
| read | search books (a word from seeded titles/authors, or one whole seeded title) | 15% |
| read | open a random book | 10% |
| read | list a random page of members, or open a member | 8% |
| read | list a random page of loans (active, returned or all) | 7% |
| write | borrow a book | 13% |
| write | return a loan | 13% |
| write | edit a book | 5% |
| write | add a book | 4% |
| write | add a member | 3% |
| write | edit a member | 2% |

So 60% of the actions are reads and 40% writes. Some actions need a supporting request:
an edit first reads the record (`edit_book_get`, `edit_member_get`), and a return picks
one of the user's own loans or, if it has none, reads a random page of active loans
(`return_lookup`). The summary therefore also shows the real split of the requests, next
to the intended split of the actions. The edits keep the copies, ISBN and email, so they
cannot clash with other data (the API has no version column; a PUT replaces the record).

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
| browse_books, search_books, open_book, list_members, open_member, list_loans | 200 |
| borrow | 201, or 409 (no copies left, or the member is at the 5-loan limit) |
| return | 200, or 409 (the loan was already returned) |
| return_lookup, edit_book_get, edit_member_get | 200 |
| edit_book, edit_member | 200 |
| add_book | 201, or 409 (duplicate ISBN) |
| add_member | 201 |

A `Rate` metric counts the errors and `responseCallback` makes k6's own `http_req_failed`
agree. The summary shows the share of each expected non-2xx outcome per action. If one is
above 5%, the summary says so at the top: the mix is then not measuring what it claims
(for example, if most returns hit 409, they are not doing the work of a return).

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

## How to run it

Needs Docker, `curl`, `jq` and `make images` (the images must exist). Nothing outside the
`lt-` prefix is touched: not your `make up` cluster, not `e2e-`.

```bash
make images
scripts/loadtest.sh up                  # or: make loadtest-up
scripts/loadtest.sh users 100           # one level; exit 0 pass, 1 fail
scripts/loadtest.sh rps 200
scripts/loadtest.sh search users        # step up from 100 by x2, then bisect, then confirm
scripts/loadtest.sh search rps 50 2     # START and FACTOR are optional
scripts/loadtest.sh down                # or: make loadtest-down
make loadtest                           # up, search users, search rps, down
```

The search steps up from START by FACTOR until a level fails, bisects between the last
pass and the first fail until the gap is at most 10% of the pass, then runs the best
pass once more to confirm it. If that fails, it steps down 10% and confirms again, at most
twice. If the first level already fails, it steps down by FACTOR until one passes.

Environment overrides: `RAMP` (60 s) and `HOLD` (300 s); `P95_MS`, `P99_MS`, `ERR_PCT`,
`DROP_PCT`; `BOOKS`, `MEMBERS`, `LOANS` (for `up`); `NODE_CPUS`, `CENTRAL_CPUS`,
`K6_CPUS`, `K6_IMAGE`, `BASE_URL`; `PAUSE_S` (30), `MAX_LEVEL` (the search stops here),
`RESULTS_DIR`.

Between levels the runner waits until the sync worker's pending changes are 0 (and
reports how long that took), then pauses 30 s.

During every hold it samples every 5 s: `docker stats` of the three node containers (CPU
summed, out of 200%, and memory), the worker's pending changes and lag, and Litestream's
position and PUT counter, from Prometheus. After each level it checks `RestartCount`
and `OOMKilled`.

### Results

Each run writes `loadtest/results/<UTC timestamp>/` (not committed): one directory per
level with `k6.log`, `k6-summary.json` (k6's own numbers), `samples.jsonl` (the raw
samples), `events.jsonl` (Docker's die and OOM events of the node containers) and `level.json`, plus `summary.json` and `summary.md`. The table has one row
per level: result and reason, requests per second, p50/p95/p99, error %, node CPU
(average and maximum, of 200%), peak memory per container, maximum sync pending and
lag, whether Litestream's PUTs kept increasing during the hold, the drain time and
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
- **A node far over capacity can be OOM-killed.** When the node is slow, the requests-per-second
  model starts more VUs, and each logs in. Every login hashes a password with Argon2
  (about 19 MiB each), and the web app does not limit how many hashes run at once. A
  level at several times the capacity can therefore push the web container over its 3 GB
  and get it killed. That is a failed level (restart or OOM), not a harness error.
- The load generator and the measuring (`docker stats`, Prometheus queries) take a little
  CPU on the host, outside the node's cores.

## Results

Not yet run.

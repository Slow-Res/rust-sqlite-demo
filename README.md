# SQLite in production

A complete, runnable example of SQLite as the production database. Each node runs a
library web app on its own SQLite file. Litestream copies every change to S3, and a
sync worker copies the changed rows to a central Postgres. A node works on its own,
even when Postgres or S3 is down.

Read [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how it fits together. The full
design is in the [spec](docs/superpowers/specs/2026-10-10-sqlite-production-design.md).

## You need

- Rust stable (see `rust-toolchain.toml`)
- Node 22
- Docker 25 or newer, with Compose v2
- `sqlx-cli`: `cargo install sqlx-cli --no-default-features --features sqlite,rustls`
- `curl` and `jq`

## Coding locally (no containers)

```bash
make setup                    # checks tools, installs npm packages, creates data/dev.db
make dev                      # the web app on :4000
make dev-frontend             # in a second terminal: React on :5173
make seed-dev                 # sample books, members and loans
make user-dev NAME=alice      # a staff account (asks for a password twice)
```

Open http://localhost:5173.

## The three-node cluster

This runs the same `deploy/compose/node.yml` that a real VM runs, three times, next
to Postgres, Garage (a local S3) and Prometheus.

```bash
make up                       # builds the images and starts everything
make user NODE=node1          # a staff account on node1
make seed NODE=node1          # sample data on node1
```

Open http://localhost:4001 (and 4002, 4003 for the other nodes). Each node is its own
library branch with its own data.

See the data arrive in Postgres, tagged with the node it came from:

```bash
make psql
```

```sql
SELECT node_id, count(*) FROM books GROUP BY 1;
```

Prometheus and its alerts are at http://127.0.0.1:9090 (alerts at `/alerts`).

```bash
make down                     # stop everything; the data is kept
make reset                    # stop everything and delete all its data (asks first)
```

## Tests

| Command | What it runs |
|---|---|
| `make test` | The Rust tests. It starts a throwaway Postgres in Docker (port 55432) for the worker tests. |
| `make check` | Every CI gate except the end-to-end test: format, clippy, tests, the "web has no Postgres driver" check, generated TypeScript types, and the React lint, typecheck and tests. |
| `make e2e` | Builds the images, then runs the restore-gate test (15 checks) and the full cluster scenarios on a separate, throwaway cluster. It does not touch your `make up` cluster. |

`make e2e` took about 9 minutes here with the images already built; a first run that
builds them takes longer. The restore-gate test
([test-restore-gate.sh](scripts/test-restore-gate.sh)) is most of that, about 8
minutes, because its unreachable-S3 cases make Litestream retry before it gives up.
It ends with `restore gate: 15 checks passed`. The
scenarios ([e2e.sh](scripts/e2e.sh)) cover writes on every node, a lost disk, a node
with no backup, Postgres down, and the restore drill.

`make restore-drill NODE=node2` restores a node's newest backup into a scratch
volume, checks it and prints its counts next to the live node's. By default it fails
only when the copy is corrupt. `scripts/restore-drill.sh <node> --strict` also fails
when any count differs. In production the live counts keep moving, so use `--strict`
only on an idle node; the e2e test uses it after waiting for the upload. See the
[runbook](docs/RUNBOOK.md#run-the-restore-drill).

## Folders

| Folder | Job |
|---|---|
| `apps/web` | The Axum web server (package `library-web`). Runs the SQLite migrations. |
| `apps/sync-worker` | Copies the change log to Postgres (package `sync-worker`). Runs the Postgres migrations. |
| `apps/frontend` | The React app. Its build is served by `web`. |
| `crates/shared` | API types shared by web and frontend; generates the TypeScript types (package `library-shared`). |
| `crates/sqlite` | The one place SQLite is opened and configured (package `sqlite-setup`). |
| `migrations/sqlite` | App tables, change log and triggers. Run by `web`. |
| `migrations/postgres` | Mirror tables, checkpoints and archive. Run by `sync-worker`. |
| `deploy/docker` | The Dockerfiles, Litestream config and the restore gate. |
| `deploy/compose` | `node.yml` (one VM), `central.yml` (Postgres, Garage, Prometheus) and the local cluster's env files. |
| `deploy/garage` | Config and setup script for the local S3 (Garage). |
| `deploy/monitoring` | Prometheus config and alert rules. |
| `scripts` | The local cluster, the end-to-end test, the restore drill and the test Postgres. |
| `docs` | Architecture, runbook and decision records. |
| `.github/workflows` | CI. |

## Ports

| What | Port |
|---|---|
| web, inside its container | 4000 |
| local nodes on the host | 4001, 4002, 4003 (bound to 127.0.0.1) |
| Litestream metrics | 9090 |
| worker health and metrics | 9100 |
| cluster Postgres on the host | `127.0.0.1:5435` |
| test Postgres | `127.0.0.1:55432` |
| Prometheus on the host | `127.0.0.1:9090` |

`node.yml` never publishes the metrics ports (9090 and 9100); Prometheus reaches them
over the Docker network. The end-to-end test uses its own ports (web 4101-4103,
Postgres 5434, Prometheus 9091), so it can run next to `make up`.

## Read next

- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): the picture, one write step by step, the guarantees.
- [docs/RUNBOOK.md](docs/RUNBOOK.md): what to do when something breaks, and how to add a node or deploy.
- [docs/decisions](docs/decisions): why it is built this way.
  - [0001 SQLite as the primary database](docs/decisions/0001-sqlite-as-the-primary-database.md)
  - [0002 One container per job](docs/decisions/0002-one-container-per-job.md)
  - [0003 Expand-and-contract migrations](docs/decisions/0003-expand-and-contract-migrations.md)
  - [0004 Change log written by triggers](docs/decisions/0004-change-log-written-by-triggers.md)
  - [0005 Tokens and epochs detect restores](docs/decisions/0005-tokens-and-epochs-detect-restores.md)
  - [0006 Garage instead of MinIO](docs/decisions/0006-garage-instead-of-minio.md)

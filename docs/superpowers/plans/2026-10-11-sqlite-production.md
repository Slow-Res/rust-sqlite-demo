# SQLite in Production Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a complete, runnable example of SQLite as the production database: several nodes, each running the library web app on its own SQLite file, backed up by Litestream to S3 and copied row by row to a central Postgres by a sync worker.

**Architecture:**
- **Cargo workspace.**
  - `apps/web` is the library app's Axum backend, copied in.
  - `apps/sync-worker` is a new Rust program.
  - `crates/shared` holds the API types.
  - `crates/sqlite` holds the SQLite settings.
  - `apps/frontend` is the React app.
- **Change capture.** SQLite triggers write a `change_log`. The worker turns it into batched upserts in Postgres, saving a checkpoint in the same transaction.
- **Restore detection.** A random token on each log row lets the worker notice that the node was restored from an older backup, and re-copy the node.
- **Deployment.** Each node is four containers (`restore`, `litestream`, `web`, `sync-worker`) defined in `deploy/compose/node.yml`. The local cluster runs that same file three times next to Postgres, Garage (S3) and Prometheus.

**Tech Stack:**

| Area | Tools |
|---|---|
| Rust | stable, edition 2024 |
| Web and data | Axum 0.8, SQLx 0.9 (SQLite + Postgres), Tokio, tokio-util |
| Observability | prometheus-client 0.25, tracing |
| Frontend | React 19 + Vite (copied) |
| Backup and services | Litestream 0.5.17, Garage v2.4.1, Postgres 18.6, Prometheus v3.13.4 |
| Running it | Docker Compose |

**Spec:** `docs/superpowers/specs/2026-10-10-sqlite-production-design.md`. It is the authority. Read the section a task names before starting it.

## Global Constraints

- **Repo:** `~/rust/rust-sqlite-demo`.
- **Source app:** `~/rust/library-web-app` at commit `9d1405e`. Copy from that commit only, with `git archive`, never from its working tree.
- **Pinned versions:**
  - Litestream `0.5.17` (`litestream/litestream:0.5.17`)
  - Garage `dxflrs/garage:v2.4.1`
  - Postgres `postgres:18.6`
  - Prometheus `prom/prometheus:v3.13.4`
  - Node `node:22-bookworm-slim`
  - `prometheus-client = "0.25"`, `axum = "0.8"`, `sqlx = "0.9"`, `tower-http = "0.7"`
- **Container user:** every container that mounts the node volume runs as uid `10001`.
- **Postgres code:** only `apps/sync-worker` enables SQLx's `postgres` feature. `apps/web` must never depend on a Postgres driver.
- **SQLite settings:** WAL, `synchronous=NORMAL`, `busy_timeout=5000`, `foreign_keys=ON`. They are set only in `crates/sqlite` (package `sqlite-setup`). Every program opens SQLite through it.
- **Migrations:**
  - Only `library-web` runs SQLite migrations (`migrations/sqlite`).
  - Only `sync-worker` runs Postgres migrations (`migrations/postgres`).
  - Never edit a migration after its task is committed.
- **`NODE_ID` format:** must match `^[a-z0-9][a-z0-9-]{0,39}$`. This is enforced in web config, worker config and `restore.sh`.
- **Ports:**

  | What | Port |
  |---|---|
  | web, inside its container | 4000 |
  | local nodes on the host | 4001, 4002, 4003 |
  | Litestream metrics | 9090 |
  | worker health and metrics | 9100 |
  | cluster Postgres on the host | `127.0.0.1:5435` |
  | test Postgres | `127.0.0.1:55432` |
  | Prometheus on the host | `127.0.0.1:9090` |

  `node.yml` never publishes the metrics ports.
- **Secrets:** the local dev credentials below are the only secrets committed, and every file holding them says "local only".
  - Garage key id `GK1a0c2d4e6f8091a2b3c4d5e6`
  - Garage secret `6c6f63616c2d6465762d6f6e6c792d6e6f742d612d7265616c2d736563726574`
  - Garage rpc secret `6761726167652d7270632d6c6f63616c2d6465762d6f6e6c792d736563726574`
  - Garage admin token `6761726167652d61646d696e2d6c6f63616c2d6465762d6f6e6c792d746f6b6e`
  - Postgres user `sync`, password `local-dev-only`, database `library`
- **Formatting:** the code blocks in this plan are not rustfmt-formatted. Run `cargo fmt` after writing Rust code and before every gate that runs `cargo fmt --check` (`make check` included). Formatting changes are expected; never change behaviour to satisfy fmt.
- **Lints:** workspace lints `unsafe_code = "forbid"`, `clippy::unwrap_used`/`expect_used = "warn"`. Tests may unwrap (`clippy.toml`). `cargo clippy --workspace --all-targets -- -D warnings` must stay clean, and so must `cargo fmt --check`.
- **Style:** match the copied library code. Module doc comments say what the module is for. Comments explain *why*. Plain words.
- **Commits:** commit at the end of every task with a conventional prefix (`feat:`, `test:`, `build:`, `docs:`, `chore:`). Do not push; there is no remote.
- **The worker never skips a change.** Any batch it cannot apply fails and is retried.

## Review Focus

These are inputs the spec implies but that are easy to forget. Each one has a test in the task named in brackets.

1. **NULL values in nullable columns:** `books.isbn`, `books.published_year`, `loans.returned_at`. They must arrive in Postgres as NULL, never `""` or `0`. Text with quotes, emoji and 2000 characters must arrive unchanged. [Task 7]
2. **Batch and chunk boundaries:** more changes than `BATCH_SIZE` (1203 changes with size 500 means three batches with no gap and no repeat), and a full copy of more than 1000 rows (2500 rows across three insert chunks). [Tasks 7, 8]
3. **A brand-new empty node:**
   - the first sync makes a checkpoint `(epoch 1, last_seq 0, token NULL)` that stays valid;
   - the next write ships normally;
   - the restore gate creates the empty database only with `ALLOW_FRESH_DB=true`.

   [Tasks 8, 10]
4. **Reused row ids:** SQLite gives a new row `max(id)+1`, so deleting the newest book and inserting another reuses its id. Postgres must end with the new row's values, whether the delete and insert land in one batch or two. [Task 8]
5. **Bad `NODE_ID` values:** `""`, `Node1`, `node_1`, `-node`, `a/b`, `../x`, a value with a newline, and 41 characters are refused everywhere a node id enters: web config, worker config and `restore.sh`. A newline must not slip past a line-based check. [Tasks 4, 5, 10]

## File map

| Path | Job | Task |
|---|---|---|
| `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `.cargo/config.toml`, `.gitignore`, `.env.example` | workspace and tool settings | 1 |
| `apps/web/**` | library backend (package `library-web`) | 1, 2, 3, 4 |
| `apps/frontend/**` | React app | 1 |
| `crates/shared/**` | API types + `is_valid_node_id` (package `library-shared`) | 1, 4 |
| `crates/sqlite/**` | `sqlite-setup`: open + check SQLite | 2 |
| `migrations/sqlite/20261010000000_init.sql` | app tables (copied) | 1 |
| `migrations/sqlite/20261011000000_change_log.sql` | change log + triggers | 3 |
| `migrations/postgres/20261011000000_mirror.sql` | mirror tables, checkpoints, archive | 5 |
| `apps/sync-worker/src/{lib,config,tables}.rs` | worker crate root, settings, synced-table list | 5 |
| `apps/sync-worker/src/changes.rs` | SQLite side: read batches and snapshots, prune | 6 |
| `apps/sync-worker/src/mirror.rs` | Postgres side: lock, apply, full copy | 7 |
| `apps/sync-worker/src/sync.rs` | one sync cycle | 8 |
| `apps/sync-worker/src/{schema,metrics,http,run,status,cli,telemetry,main}.rs` | runtime: loop, retries, metrics, CLI | 9 |
| `apps/sync-worker/tests/**` | worker tests | 5–9 |
| `scripts/test-postgres.sh`, `Makefile` | throwaway test Postgres, commands | 1, 5, 11 |
| `deploy/docker/Dockerfile`, `deploy/docker/litestream.Dockerfile`, `deploy/docker/litestream/{litestream.yml,restore.sh}`, `.dockerignore`, `scripts/test-restore-gate.sh` | images and restore gate | 10 |
| `deploy/compose/{node.yml,node.env.example,central.yml}`, `deploy/compose/local/*`, `deploy/garage/*`, `deploy/monitoring/*`, `scripts/cluster.sh` | running nodes and the local cluster | 11 |
| `scripts/e2e.sh`, `scripts/restore-drill.sh` | end-to-end test, restore drill | 12 |
| `docs/ARCHITECTURE.md`, `docs/RUNBOOK.md`, `docs/decisions/*`, `README.md`, `.github/workflows/ci.yml` | documents and CI | 13 |

---

### Task 1: Workspace with the library app copied in

Move the library app into the new layout with no behaviour change. Every copied test must still pass.

**Files:**
- Create: `Cargo.toml`, `.cargo/config.toml`, `Makefile`
- Create (copied): `apps/web/**`, `apps/frontend/**`, `crates/shared/**`, `migrations/sqlite/20261010000000_init.sql`, `Cargo.lock`, `rust-toolchain.toml`, `clippy.toml`, `rustfmt.toml`, `.gitignore`, `.env.example`
- Modify (after copy): `apps/web/Cargo.toml`, `apps/web/src/db.rs`, every file under `apps/web` that says `library_backend` or `library-backend`, every `#[sqlx::test]` in `apps/web`, `crates/shared/src/lib.rs`, `crates/shared/Cargo.toml` (feature comment), `.env.example` (log filter and paths)

**Interfaces:**
- Produces:
  - packages `library-web` (lib `library_web`, bin `library-web`) and `library-shared`;
  - SQLite migrations embedded with `sqlx::migrate!("../../migrations/sqlite")`;
  - test attribute `#[sqlx::test(migrations = "../../migrations/sqlite")]`.

- [ ] **Step 1: Record the baseline test count in the source app**

```bash
cd ~/rust/library-web-app && git log --oneline -1   # expect 9d1405e
SQLX_OFFLINE=true cargo test -p library-backend -p library-shared 2>&1 | grep -E '^test result' | awk '{s+=$4} END {print "passed:", s}'
```
Write the number down. Step 9 must report the same total.

- [ ] **Step 2: Copy the tracked files from commit 9d1405e**

```bash
cd ~/rust/rust-sqlite-demo
SRC=~/rust/library-web-app
TMP=$(mktemp -d)
git -C "$SRC" archive 9d1405e shared backend frontend-react Cargo.lock rust-toolchain.toml clippy.toml rustfmt.toml .gitignore .env.example | tar -x -C "$TMP"
mkdir -p apps crates migrations
mv "$TMP/shared" crates/shared
mv "$TMP/backend/migrations" migrations/sqlite
mv "$TMP/backend" apps/web
mv "$TMP/frontend-react" apps/frontend
mv "$TMP/Cargo.lock" "$TMP/rust-toolchain.toml" "$TMP/clippy.toml" "$TMP/rustfmt.toml" "$TMP/.gitignore" "$TMP/.env.example" .
rm -rf "$TMP"
ls apps/web/.sqlx | head -3   # the offline query data came along
```

- [ ] **Step 3: Write the workspace manifest**

`Cargo.toml`:
```toml
[workspace]
resolver = "3"
members = ["crates/shared", "apps/web"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"

# Versions live here once; member crates opt in with `dep.workspace = true`.
[workspace.dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
chrono = { version = "0.4", features = ["serde"] }
ts-rs = { version = "12", features = ["chrono-impl", "serde-compat"] }
library-shared = { path = "crates/shared" }
# Base features only. Each member adds what it needs (the web app: chrono,
# migrate, macros; the worker: postgres). A crate that is built on its own
# gets only its own features, which keeps Postgres out of the web binary.
sqlx = { version = "0.9", features = ["runtime-tokio", "sqlite"] }
argon2 = "0.6"
cookie = "0.18"
async-trait = "0.1"
axum-extra = { version = "0.12", features = ["cookie"] }
hex = "0.4"
rand = "0.10"
rpassword = "7"
sha2 = "0.11"

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
unwrap_used = "warn"
expect_used = "warn"
```

`rust-toolchain.toml`. The wasm target is gone with Leptos:
```toml
[toolchain]
channel = "stable"
components = ["rustfmt", "clippy"]
```

`.cargo/config.toml`:
```toml
# Where `cargo test -p library-shared --features ts` writes the generated TypeScript.
[env]
TS_RS_EXPORT_DIR = { value = "apps/frontend/src/generated", relative = true }
```

- [ ] **Step 4: Rename the backend package and point it at the new migrations folder**

In `apps/web/Cargo.toml`:
- set `name = "library-web"` under `[package]`;
- set `name = "library_web"` under `[lib]`;
- set `name = "library-web"` under `[[bin]]`;
- replace the `sqlx = { version = "0.9", ... }` line with:

```toml
sqlx = { workspace = true, features = ["chrono", "migrate", "macros"] }
```
Then:
```bash
cd ~/rust/rust-sqlite-demo
grep -rl 'library_backend' apps/web | xargs sed -i 's/library_backend/library_web/g'
grep -rl 'library-backend' apps/web | xargs sed -i 's/library-backend/library-web/g'
sed -i 's/library_backend/library_web/g' .env.example   # the RUST_LOG filter names the crate
sed -i 's|sqlx::migrate!()|sqlx::migrate!("../../migrations/sqlite")|' apps/web/src/db.rs
# Only attribute lines, not comments that mention the attribute:
grep -rlE '^\s*#\[sqlx::test\]$' apps/web | xargs sed -i -E 's|^(\s*)#\[sqlx::test\]$|\1#[sqlx::test(migrations = "../../migrations/sqlite")]|'
grep -rn 'sqlx::test\]' apps/web    # only comments may remain; reword each to say `#[sqlx::test(...)]`
grep -rn 'migrations/' apps/web/tests apps/web/src | grep -v '../../migrations/sqlite'   # fix any prose that says `migrations/` to `migrations/sqlite/`
```

- [ ] **Step 5: Remove Leptos and old paths from the copied text**

```bash
grep -rniE 'leptos|frontend-react|both frontends|backend/' crates apps/web .env.example Makefile 2>/dev/null
```
Edit each hit:
- `frontend-react/src/generated` becomes `apps/frontend/src/generated`.
- "both frontends" becomes "the frontend".
- Leptos mentions are deleted.
- `backend/` paths become `apps/web/`.

The `ts` feature comment in `crates/shared/Cargo.toml` should now say it is off by default so normal builds skip ts-rs.

- [ ] **Step 6: Write the starting Makefile**

`Makefile`. Recipe lines start with a TAB:
```make
.PHONY: setup db dev dev-frontend seed-dev user-dev test lint fmt types sqlx-prepare check

# One-time developer setup. Tools are not installed for you: missing ones are reported.
setup: ## Check tools, install npm deps, create the dev database
	@command -v sqlx >/dev/null || echo "missing sqlx-cli: cargo install sqlx-cli --no-default-features --features sqlite,rustls"
	@command -v docker >/dev/null || echo "missing docker (needed for tests that use Postgres and for the cluster)"
	@test -f .env || { cp .env.example .env; echo "created .env from .env.example"; }
	cd apps/frontend && npm ci
	$(MAKE) db

db: ## Create and migrate the dev database
	mkdir -p data
	sqlx database create
	sqlx migrate run --source migrations/sqlite

dev: ## Web app on :4000 against data/dev.db (no containers)
	cargo run -p library-web

dev-frontend: ## React dev server on :5173 (proxies /api to :4000)
	cd apps/frontend && npm run dev

seed-dev: ## Sample data in the dev database
	cargo run -p library-web -- seed

# Usage: make user-dev NAME=alice   (prompts for the password twice)
user-dev: ## Staff account in the dev database
	cargo run -p library-web -- create-user $(or $(NAME),admin)

test: ## Rust tests
	cargo test --workspace

lint:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt

types: ## Regenerate TypeScript types from the shared crate
	cargo test -p library-shared --features ts

# sqlx-cli runs from apps/web/, so give it an absolute path to the dev DB.
sqlx-prepare: ## Refresh committed offline query metadata
	cd apps/web && DATABASE_URL=sqlite://$(CURDIR)/data/dev.db cargo sqlx prepare

check: ## Every gate CI runs
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	SQLX_OFFLINE=true cargo build -p library-web
	cd apps/frontend && npm run lint && npm run typecheck && npm test
```

- [ ] **Step 7: Build offline and run the Rust tests**

```bash
cd ~/rust/rust-sqlite-demo
SQLX_OFFLINE=true cargo build -p library-web
SQLX_OFFLINE=true cargo test --workspace 2>&1 | grep -E '^test result' | awk '{s+=$4} END {print "passed:", s}'
```
Expected: the build succeeds, and "passed:" equals the Step 1 number.

- [ ] **Step 8: Check the generated TypeScript and the frontend**

```bash
git add -A && git commit -qm "chore: copy the library app into the new layout (unchanged behaviour)"
cargo test -p library-shared --features ts && git status --porcelain apps/frontend/src/generated   # expect no output
cd apps/frontend && npm ci && npm run lint && npm run typecheck && npm test && npm run build && cd ../..
```
Expected: `git status` prints nothing, which means the types are unchanged, and every npm command passes.

- [ ] **Step 9: Run the full gate and amend the commit**

```bash
cargo fmt   # the renames change line lengths in apps/web/src/cli.rs
make check
cargo clippy --workspace --all-targets -- -D warnings
git add -A && git commit -q --amend -m "chore: copy the library app into the new layout (unchanged behaviour)"
```

---

### Task 2: `crates/sqlite`, the one place SQLite is configured

Spec §5 rules S2–S5, S8, S15.

**Files:**
- Create: `crates/sqlite/Cargo.toml`, `crates/sqlite/src/lib.rs`
- Modify: `Cargo.toml` (members + workspace dependency), `apps/web/Cargo.toml`, `apps/web/src/db.rs:14-42`

**Interfaces:**
- Produces, used by Tasks 4, 5 and 9:
  - `sqlite_setup::BUSY_TIMEOUT: Duration`
  - `sqlite_setup::connect_options(url: &str, create_if_missing: bool) -> anyhow::Result<SqliteConnectOptions>`
  - `sqlite_setup::open_pool(url: &str, max_connections: u32, create_if_missing: bool) -> anyhow::Result<SqlitePool>`
  - `sqlite_setup::check(pool: &SqlitePool) -> anyhow::Result<CheckReport>`
  - `pub struct CheckReport { pub integrity_errors: Vec<String>, pub foreign_key_errors: Vec<String> }` with `fn is_ok(&self) -> bool`

- [ ] **Step 1: Create the crate manifest and register it**

`crates/sqlite/Cargo.toml`:
```toml
[package]
name = "sqlite-setup"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
sqlx.workspace = true
anyhow = "1"

[dev-dependencies]
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
tempfile = "3"

[lints]
workspace = true
```
In root `Cargo.toml`:
- set `members = ["crates/shared", "crates/sqlite", "apps/web"]`;
- add `sqlite-setup = { path = "crates/sqlite" }` under `[workspace.dependencies]`.

- [ ] **Step 2: Write the failing tests**

`crates/sqlite/src/lib.rs`, with only the test module for now:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn temp_url(dir: &tempfile::TempDir, name: &str) -> String {
        format!("sqlite://{}", dir.path().join(name).display())
    }

    #[tokio::test]
    async fn every_connection_gets_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_pool(&temp_url(&dir, "t.db"), 2, true).await.unwrap();
        // Hold two connections at once so the pool has to open a second one.
        let mut first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        for conn in [&mut first, &mut second] {
            let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(mode, "wal");
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(synchronous, 1, "1 = NORMAL");
            let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(busy, 5000);
            let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(foreign_keys, 1);
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_error_without_create() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_pool(&temp_url(&dir, "nope.db"), 1, false)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("nope.db"), "{err:#}");
        assert!(!dir.path().join("nope.db").exists());
    }

    #[tokio::test]
    async fn create_makes_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("a/b/t.db").display());
        open_pool(&url, 1, true).await.unwrap();
        assert!(dir.path().join("a/b/t.db").exists());
    }

    #[tokio::test]
    async fn check_passes_on_a_healthy_database() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_pool(&temp_url(&dir, "t.db"), 1, true).await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        let report = check(&pool).await.unwrap();
        assert!(report.is_ok(), "{report:?}");
    }

    #[tokio::test]
    async fn check_reports_broken_references() {
        let dir = tempfile::tempdir().unwrap();
        // One connection, so the PRAGMA below and the insert share it.
        let pool = open_pool(&temp_url(&dir, "t.db"), 1, true).await.unwrap();
        for sql in [
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id))",
            "PRAGMA foreign_keys = OFF",
            "INSERT INTO child (id, parent_id) VALUES (1, 42)",
            "PRAGMA foreign_keys = ON",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let report = check(&pool).await.unwrap();
        assert!(!report.is_ok());
        assert!(report.integrity_errors.is_empty());
        assert_eq!(
            report.foreign_key_errors,
            vec!["child row 1 points to a missing row in parent".to_owned()]
        );
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test -p sqlite-setup`
Expected: compile errors, because `open_pool` and `check` are not defined.

- [ ] **Step 4: Implement the crate**

Put this above the test module in `crates/sqlite/src/lib.rs`:
```rust
//! The one place SQLite is opened and configured (spec §5).
//!
//! Both programs that open a node's database file, `library-web` and
//! `sync-worker`, go through [`open_pool`], so they can never disagree on the
//! settings below. The settings live in the connect options, which the pool
//! applies to *every* connection it opens, not just the first (rule S8).

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

/// How long a connection waits for the write lock before failing with
/// `SQLITE_BUSY` (rule S4).
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect options with the production settings applied:
///
/// - WAL journal: readers never block the writer, and Litestream needs it (S2).
/// - `synchronous = NORMAL`: safe with WAL. A power cut can lose the last
///   commits but never corrupts the file (S3).
/// - `busy_timeout`: wait for the write lock instead of failing at once (S4).
/// - `foreign_keys = ON`: SQLite leaves it off unless each connection asks (S5).
///
/// Write transactions should start with `pool.begin_with("BEGIN IMMEDIATE")`.
/// A plain `BEGIN` that reads and then writes can fail with `SQLITE_BUSY`
/// halfway when another connection took the write lock in between (S6).
pub fn connect_options(url: &str, create_if_missing: bool) -> anyhow::Result<SqliteConnectOptions> {
    let options = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("invalid SQLite URL `{url}`"))?
        .create_if_missing(create_if_missing)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(BUSY_TIMEOUT);
    Ok(options)
}

/// Open a pool on `url`. With `create_if_missing`, also create the file and
/// its parent directory; without it, a missing file is an error.
pub async fn open_pool(
    url: &str,
    max_connections: u32,
    create_if_missing: bool,
) -> anyhow::Result<SqlitePool> {
    let options = connect_options(url, create_if_missing)?;
    if create_if_missing
        && let Some(dir) = options
            .get_filename()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }
    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .with_context(|| format!("opening SQLite database `{url}`"))
}

/// Result of [`check`]. Empty lists mean the database is healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Lines from `PRAGMA integrity_check` other than its single `ok`.
    pub integrity_errors: Vec<String>,
    /// One line per row from `PRAGMA foreign_key_check`.
    pub foreign_key_errors: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.integrity_errors.is_empty() && self.foreign_key_errors.is_empty()
    }
}

/// Look for corruption and broken references (rule S15). It reads the whole
/// file, so it takes a while on a big database.
pub async fn check(pool: &SqlitePool) -> anyhow::Result<CheckReport> {
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(pool)
        .await
        .context("running PRAGMA integrity_check")?;
    let integrity_errors = integrity.into_iter().filter(|line| line != "ok").collect();

    let rows = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .context("running PRAGMA foreign_key_check")?;
    let foreign_key_errors = rows
        .iter()
        .map(|row| -> Result<String, sqlx::Error> {
            let table: String = row.try_get(0)?;
            let rowid: Option<i64> = row.try_get(1)?;
            let parent: String = row.try_get(2)?;
            let rowid = rowid.map_or_else(|| "?".to_owned(), |id| id.to_string());
            Ok(format!("{table} row {rowid} points to a missing row in {parent}"))
        })
        .collect::<Result<Vec<_>, _>>()
        .context("reading PRAGMA foreign_key_check")?;

    Ok(CheckReport {
        integrity_errors,
        foreign_key_errors,
    })
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p sqlite-setup`
Expected: 5 passed.

- [ ] **Step 6: Make the web app open SQLite through the crate**

In `apps/web/Cargo.toml` add `sqlite-setup.workspace = true` under `[dependencies]`. Replace `connect` in `apps/web/src/db.rs` with the code below. Remove the now-unused imports (`FromStr`, `Duration`, the `sqlx::sqlite::*` options). Keep `migrate`, `RepoError` and the rest unchanged.
```rust
/// Open the pool, creating the database file and its parent directory if
/// needed. The settings (WAL, busy timeout, foreign keys...) come from
/// `sqlite-setup`, the one place they are defined (spec §5).
pub async fn connect(config: &Config) -> anyhow::Result<SqlitePool> {
    sqlite_setup::open_pool(&config.database_url, MAX_CONNECTIONS, true).await
}
```

- [ ] **Step 7: Run everything and commit**

```bash
SQLX_OFFLINE=true cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
git add -A && git commit -qm "feat: sqlite-setup crate, the one place SQLite is configured"
```
Expected: all tests pass. The total is the Task 1 number plus 5.

---

### Task 3: The change log and its triggers

Spec §6.1.

**Files:**
- Create: `migrations/sqlite/20261011000000_change_log.sql`, `apps/web/build.rs`
- Test: `apps/web/tests/change_log.rs`

**Interfaces:**
- Produces, used by Tasks 5–9:
  - table `change_log(seq, table_name, row_id, op, token, created_at)`;
  - triggers named `<table>_logged_insert`, `<table>_logged_update`, `<table>_logged_delete` on `books`, `members` and `loans`, and on no other table.

- [ ] **Step 1: Write the failing tests**

`apps/web/tests/change_log.rs`:
```rust
//! The change log (spec §6.1): every committed write to a synced table leaves
//! exactly one row per changed row id, written by triggers in the same
//! transaction.
#![allow(clippy::unwrap_used)]

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use sqlx::SqlitePool;

/// `(table_name, row_id, op)` for every change_log row, oldest first.
async fn log(pool: &SqlitePool) -> Vec<(String, i64, String)> {
    sqlx::query_as("SELECT table_name, row_id, op FROM change_log ORDER BY seq")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn entry(table: &str, id: i64, op: &str) -> (String, i64, String) {
    (table.to_owned(), id, op.to_owned())
}

async fn insert_book(pool: &SqlitePool) -> i64 {
    sqlx::query("INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) VALUES ('T', 'A', NULL, NULL, 1, 0, 0)")
        .execute(pool).await.unwrap().last_insert_rowid()
}

async fn insert_member(pool: &SqlitePool, email: &str) -> i64 {
    sqlx::query("INSERT INTO members (name, email, created_at, updated_at) VALUES ('M', ?, 0, 0)")
        .bind(email).execute(pool).await.unwrap().last_insert_rowid()
}

async fn insert_loan(pool: &SqlitePool, book_id: i64, member_id: i64) -> i64 {
    sqlx::query("INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES (?, ?, 0, 100, NULL)")
        .bind(book_id).bind(member_id).execute(pool).await.unwrap().last_insert_rowid()
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn book_insert_update_delete_are_logged(pool: SqlitePool) {
    let id = insert_book(&pool).await;
    sqlx::query("UPDATE books SET title = 'U' WHERE id = ?").bind(id).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM books WHERE id = ?").bind(id).execute(&pool).await.unwrap();
    assert_eq!(
        log(&pool).await,
        vec![entry("books", id, "insert"), entry("books", id, "update"), entry("books", id, "delete")]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn member_and_loan_writes_are_logged(pool: SqlitePool) {
    let book = insert_book(&pool).await;
    let member = insert_member(&pool, "a@example.com").await;
    let loan = insert_loan(&pool, book, member).await;
    sqlx::query("UPDATE members SET name = 'N' WHERE id = ?").bind(member).execute(&pool).await.unwrap();
    sqlx::query("UPDATE loans SET returned_at = 50 WHERE id = ?").bind(loan).execute(&pool).await.unwrap();
    sqlx::query("DELETE FROM loans WHERE id = ?").bind(loan).execute(&pool).await.unwrap();
    assert_eq!(
        log(&pool).await,
        vec![
            entry("books", book, "insert"),
            entry("members", member, "insert"),
            entry("loans", loan, "insert"),
            entry("members", member, "update"),
            entry("loans", loan, "update"),
            entry("loans", loan, "delete"),
        ]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn cascaded_loan_deletes_are_logged(pool: SqlitePool) {
    let book = insert_book(&pool).await;
    let member = insert_member(&pool, "a@example.com").await;
    let loan = insert_loan(&pool, book, member).await;
    sqlx::query("DELETE FROM change_log").execute(&pool).await.unwrap();
    // loans.book_id is ON DELETE CASCADE: deleting the book deletes the loan.
    sqlx::query("DELETE FROM books WHERE id = ?").bind(book).execute(&pool).await.unwrap();
    let mut entries = log(&pool).await;
    entries.sort();
    assert_eq!(entries, vec![entry("books", book, "delete"), entry("loans", loan, "delete")]);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn changing_an_id_logs_the_old_id_as_deleted(pool: SqlitePool) {
    let id = insert_book(&pool).await;
    sqlx::query("DELETE FROM change_log").execute(&pool).await.unwrap();
    sqlx::query("UPDATE books SET id = 99 WHERE id = ?").bind(id).execute(&pool).await.unwrap();
    assert_eq!(log(&pool).await, vec![entry("books", 99, "update"), entry("books", id, "delete")]);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn users_and_sessions_are_not_logged(pool: SqlitePool) {
    let user = sqlx::query("INSERT INTO users (username, password_hash, created_at) VALUES ('u', 'h', 0)")
        .execute(&pool).await.unwrap().last_insert_rowid();
    sqlx::query("INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES ('t', ?, 0, 10)")
        .bind(user).execute(&pool).await.unwrap();
    assert!(log(&pool).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn each_entry_has_a_random_token_and_a_millisecond_time(pool: SqlitePool) {
    let now_ms = || i64::try_from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap();
    let before = now_ms();
    insert_book(&pool).await;
    insert_book(&pool).await;
    let after = now_ms();
    let rows: Vec<(Vec<u8>, i64)> = sqlx::query_as("SELECT token, created_at FROM change_log ORDER BY seq")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0.len(), 8);
    assert_ne!(rows[0].0, rows[1].0, "tokens are random");
    for (_, created_at) in rows {
        assert!((before..=after).contains(&created_at), "{before} <= {created_at} <= {after}");
    }
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn sequence_numbers_are_never_reused(pool: SqlitePool) {
    insert_book(&pool).await;
    let first: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log").fetch_one(&pool).await.unwrap();
    sqlx::query("DELETE FROM change_log").execute(&pool).await.unwrap();
    insert_book(&pool).await;
    let second: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log").fetch_one(&pool).await.unwrap();
    assert!(second > first, "AUTOINCREMENT must not reuse {first}");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn creating_a_book_through_the_api_logs_one_insert(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let body = json!({ "title": "Dune", "author": "Frank Herbert", "isbn": null, "published_year": null, "total_copies": 1 });
    let (status, _, book) = app.post_json("/api/books", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{book}");
    assert_eq!(log(&pool).await, vec![entry("books", book["id"].as_i64().unwrap(), "insert")]);
}
```
`cargo fmt` will rewrap the long lines. That is fine.

- [ ] **Step 2: Run the tests to see them fail**

Run: `SQLX_OFFLINE=true cargo test -p library-web --test change_log`
Expected: FAIL with `no such table: change_log`.

- [ ] **Step 3: Write the migration, and make new migration files trigger a rebuild**

On stable Rust, `sqlx::migrate!` and `#[sqlx::test(migrations = ...)]` do not notice a *new* file in the migrations folder, so the already-built test binary would keep the old migration list. A build script that watches the folder fixes that for this task and every later migration (this is what `sqlx migrate build-script` generates).

`apps/web/build.rs`:
```rust
//! Rebuild when a migration is added: `sqlx::migrate!` embeds the folder at
//! compile time but does not watch it for new files on stable Rust.
fn main() {
    println!("cargo:rerun-if-changed=../../migrations/sqlite");
}
```

`migrations/sqlite/20261011000000_change_log.sql`:
```sql
-- The change log (spec §6.1). Triggers add one row for every insert, update or
-- delete on a synced table, inside the same transaction as the write, so a
-- change is logged if and only if it is committed. The sync worker reads this
-- table and copies the changed rows to Postgres.
--
-- The log says WHICH row changed, not what it contains: the worker reads the
-- row's current values when it ships them. Adding a column to a synced table
-- therefore needs no trigger change.
CREATE TABLE change_log (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,  -- AUTOINCREMENT: a number is never reused
    table_name TEXT    NOT NULL,
    row_id     INTEGER NOT NULL,
    op         TEXT    NOT NULL CHECK (op IN ('insert', 'update', 'delete')),  -- for people reading the log
    token      BLOB    NOT NULL DEFAULT (randomblob(8)),  -- random fingerprint that detects a restored database (spec §6.4)
    created_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER))  -- Unix ms, for lag
);

CREATE TRIGGER books_logged_insert AFTER INSERT ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'insert');
END;
CREATE TRIGGER books_logged_update AFTER UPDATE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', NEW.id, 'update');
    -- If the id itself changed, the row under the old id is gone.
    INSERT INTO change_log (table_name, row_id, op) SELECT 'books', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER books_logged_delete AFTER DELETE ON books BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('books', OLD.id, 'delete');
END;

CREATE TRIGGER members_logged_insert AFTER INSERT ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', NEW.id, 'insert');
END;
CREATE TRIGGER members_logged_update AFTER UPDATE ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', NEW.id, 'update');
    INSERT INTO change_log (table_name, row_id, op) SELECT 'members', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER members_logged_delete AFTER DELETE ON members BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('members', OLD.id, 'delete');
END;

-- Cascaded deletes (deleting a book or member deletes its loans) fire these too.
CREATE TRIGGER loans_logged_insert AFTER INSERT ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', NEW.id, 'insert');
END;
CREATE TRIGGER loans_logged_update AFTER UPDATE ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', NEW.id, 'update');
    INSERT INTO change_log (table_name, row_id, op) SELECT 'loans', OLD.id, 'delete' WHERE OLD.id <> NEW.id;
END;
CREATE TRIGGER loans_logged_delete AFTER DELETE ON loans BEGIN
    INSERT INTO change_log (table_name, row_id, op) VALUES ('loans', OLD.id, 'delete');
END;
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `SQLX_OFFLINE=true cargo test -p library-web --test change_log`
Expected: 8 passed.

- [ ] **Step 5: Run the whole suite and commit**

Migrations are embedded at compile time; `apps/web/build.rs` makes the web crate rebuild when the folder changes. No `query!` macro changed, so `.sqlx` stays as it is.
```bash
SQLX_OFFLINE=true cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
git add -A && git commit -qm "feat: change log written by triggers on books, members and loans"
```

---

### Task 4: Node id, health details and `check-db` in the web app

Spec §8 (health), §5 S15 (check), §7.3 (drill uses `check-db`).

**Files:**
- Modify:
  - `crates/shared/src/common.rs`: `Health` and new `is_valid_node_id`
  - `apps/web/src/config.rs`
  - `apps/web/src/health.rs`
  - `apps/web/src/cli.rs`
  - `apps/web/src/commands.rs`
  - `apps/web/src/main.rs`
  - `apps/web/tests/common/mod.rs`: `node_id` in the test config
  - `apps/web/tests/health.rs`
  - `apps/frontend/src/generated/Health.ts`: regenerated
  - `.env.example`
- Test: `apps/web/tests/check_db.rs`

**Interfaces:**
- Consumes: `sqlite_setup::{open_pool, check, CheckReport}` (Task 2).
- Produces:
  - `library_shared::is_valid_node_id(id: &str) -> bool`, used by Task 5;
  - `Health { status, node_id, version }`;
  - `Config.node_id: String`;
  - CLI `library-web check-db`, used by Task 12. It prints `database: <url>`, `integrity: ok`, `foreign keys: ok`, then `books: N`, `members: N`, `loans: N`, `users: N`, and exits non-zero on problems. It never creates or migrates the file.

- [ ] **Step 1: Write the failing shared and config tests**

Add to the `tests` module in `crates/shared/src/common.rs`:
```rust
    #[test]
    fn node_ids_that_are_accepted() {
        for id in ["local", "node1", "node-2", "a", "0branch", &"a".repeat(40)] {
            assert!(is_valid_node_id(id), "{id:?} should be valid");
        }
    }

    #[test]
    fn node_ids_that_are_refused() {
        for id in ["", "-node", "Node1", "node_1", "node 1", "a/b", "../x", "node1\nx", "nöde", &"a".repeat(41)] {
            assert!(!is_valid_node_id(id), "{id:?} should be refused");
        }
    }
```
Add to the `tests` module in `apps/web/src/config.rs`:
```rust
    #[test]
    fn node_id_defaults_to_local() {
        let config = Config::from_lookup(lookup(&[])).unwrap();
        assert_eq!(config.node_id, "local");
    }

    #[test]
    fn bad_node_id_is_refused_with_its_name() {
        for bad in ["", "Node1", "a/b", "node1\nx"] {
            let err = Config::from_lookup(lookup(&[("NODE_ID", bad)])).unwrap_err();
            assert!(err.to_string().contains("NODE_ID"), "{err}");
        }
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `SQLX_OFFLINE=true cargo test -p library-shared -p library-web --lib`
Expected: compile errors, because `is_valid_node_id` and `node_id` are missing.

- [ ] **Step 3: Implement the node id rule, config and health**

In `crates/shared/src/common.rs`, replace `Health` and add the function:
```rust
/// Body of `GET /api/health`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Health {
    pub status: String,
    /// Which node answered (spec §8): handy when checking sticky routing.
    pub node_id: String,
    pub version: String,
}

/// A node name: 1-40 characters of `a-z`, `0-9` and `-`, not starting with
/// `-`. It becomes the node's S3 folder and its Postgres `node_id`, so
/// anything else (slashes, spaces, capitals, newlines) is refused.
pub fn is_valid_node_id(id: &str) -> bool {
    let allowed = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit();
    let mut chars = id.chars();
    matches!(chars.next(), Some(first) if allowed(first))
        && id.len() <= 40
        && chars.all(|c| allowed(c) || c == '-')
}
```
In `apps/web/src/config.rs`:
- add `pub node_id: String,` to `Config` with the doc comment `/// This node's name (spec §9); shown by /api/health.`;
- in `from_lookup`, before `Ok(Self {`, add the code below;
- add `node_id,` to the struct literal.
```rust
        let node_id = get("NODE_ID").unwrap_or_else(|| "local".to_owned());
        if !library_shared::is_valid_node_id(&node_id) {
            bail!(
                "NODE_ID must be 1-40 characters of a-z, 0-9 and `-`, starting with a letter or digit; got `{node_id}`"
            );
        }
```
In `apps/web/src/health.rs`, replace the handler:
```rust
async fn health(
    State(pool): State<SqlitePool>,
    State(config): State<Arc<Config>>,
) -> (StatusCode, ApiJson<Health>) {
    let health = |status: &str| Health {
        status: status.to_owned(),
        node_id: config.node_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (StatusCode::OK, ApiJson(health("ok"))),
        Err(err) => {
            tracing::error!(error = ?err, "health check failed");
            (StatusCode::SERVICE_UNAVAILABLE, ApiJson(health("unavailable")))
        }
    }
}
```
Add `use std::sync::Arc;` and `use crate::config::Config;` to `health.rs`.

In `apps/web/tests/common/mod.rs`:
- add `node_id: "test-node".to_owned(),` to the `Config { ... }` literal in `test_config()`;
- make `test_config` `pub` if it is not.

In `apps/web/tests/health.rs`, change the expected body:
```rust
    assert_eq!(
        body,
        json!({ "status": "ok", "node_id": "test-node", "version": env!("CARGO_PKG_VERSION") })
    );
```
Fix any other place that builds `Config { .. }` or `Health { .. }` the compiler points at. Add a `NODE_ID=local` line with a comment `# This node's name: 1-40 characters of a-z, 0-9 and '-'.` to `.env.example`.

- [ ] **Step 4: Run the tests to see them pass**

Run: `SQLX_OFFLINE=true cargo test -p library-shared -p library-web`
Expected: all pass, including `health_is_ok` with the new body.

- [ ] **Step 5: Write the failing `check-db` tests**

`apps/web/tests/check_db.rs`:
```rust
//! `library-web check-db` (spec §5 S15, §7.3).
#![allow(clippy::unwrap_used)]

mod common;

use library_web::commands::{self, inspect, render};
use library_web::config::Config;
use sqlx::SqlitePool;

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn a_seeded_database_is_ok_and_counted(pool: SqlitePool) {
    library_web::seed::seed(&pool, chrono::Utc::now()).await.unwrap();
    let inspection = inspect(&pool).await.unwrap();
    assert!(inspection.report.is_ok());
    assert_eq!(
        inspection.counts,
        vec![("books", 12), ("members", 6), ("loans", 3), ("users", 0)]
    );
    let text = render("sqlite://x.db", &inspection);
    assert_eq!(
        text,
        "database: sqlite://x.db\nintegrity: ok\nforeign keys: ok\nbooks: 12\nmembers: 6\nloans: 3\nusers: 0\n"
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn broken_references_are_reported(pool: SqlitePool) {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await.unwrap();
    sqlx::query("INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES (999, 999, 0, 1)")
        .execute(&mut *conn).await.unwrap();
    sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await.unwrap();
    drop(conn);
    let inspection = inspect(&pool).await.unwrap();
    assert!(!inspection.report.is_ok());
    assert!(render("x", &inspection).contains("foreign keys: loans row 1 points to a missing row in"));
}

#[tokio::test]
async fn a_missing_file_is_an_error_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restored.db");
    let config = Config {
        database_url: format!("sqlite://{}", path.display()),
        ..common::test_config()
    };
    assert!(commands::check_db(config).await.is_err());
    assert!(!path.exists(), "check-db must never create a database");
}
```
Add `tempfile = "3"` to `[dev-dependencies]` in `apps/web/Cargo.toml`.

- [ ] **Step 6: Run them to see them fail**

Run: `SQLX_OFFLINE=true cargo test -p library-web --test check_db`
Expected: compile errors, because `inspect`, `render` and `check_db` are missing.

- [ ] **Step 7: Implement `check-db`**

Add to `apps/web/src/commands.rs`, and add `use sqlite_setup::CheckReport;` and `use sqlx::SqlitePool;` to its imports:
```rust
/// Tables whose row counts `check-db` prints, so a restored copy can be
/// compared with the live database.
const COUNTED_TABLES: [(&str, &str); 4] = [
    ("books", "SELECT COUNT(*) FROM books"),
    ("members", "SELECT COUNT(*) FROM members"),
    ("loans", "SELECT COUNT(*) FROM loans"),
    ("users", "SELECT COUNT(*) FROM users"),
];

/// What `check-db` found.
#[derive(Debug)]
pub struct DbInspection {
    pub report: CheckReport,
    pub counts: Vec<(&'static str, i64)>,
}

pub async fn inspect(pool: &SqlitePool) -> anyhow::Result<DbInspection> {
    let report = sqlite_setup::check(pool).await?;
    let mut counts = Vec::new();
    for (table, sql) in COUNTED_TABLES {
        let count: i64 = sqlx::query_scalar(sql)
            .fetch_one(pool)
            .await
            .with_context(|| format!("counting {table}"))?;
        counts.push((table, count));
    }
    Ok(DbInspection { report, counts })
}

pub fn render(database_url: &str, inspection: &DbInspection) -> String {
    let mut out = format!("database: {database_url}\n");
    let section = |out: &mut String, name: &str, errors: &[String]| {
        if errors.is_empty() {
            out.push_str(&format!("{name}: ok\n"));
        }
        for error in errors {
            out.push_str(&format!("{name}: {error}\n"));
        }
    };
    section(&mut out, "integrity", &inspection.report.integrity_errors);
    section(&mut out, "foreign keys", &inspection.report.foreign_key_errors);
    for (table, count) in &inspection.counts {
        out.push_str(&format!("{table}: {count}\n"));
    }
    out
}

/// `check-db`: look for corruption and broken references, and print row
/// counts (spec §5 S15). It never creates or migrates the database, because it
/// is used on restored copies, which must be inspected exactly as they are.
pub async fn check_db(config: Config) -> anyhow::Result<()> {
    let pool = sqlite_setup::open_pool(&config.database_url, 1, false).await?;
    let inspection = inspect(&pool).await;
    pool.close().await;
    let inspection = inspection?;
    print!("{}", render(&config.database_url, &inspection));
    if !inspection.report.is_ok() {
        bail!("database check failed");
    }
    Ok(())
}
```
In `apps/web/src/cli.rs`:
- add the variant below to `Command`;
- add the test `fn check_db_subcommand() { assert_eq!(Cli::parse_from(["library-web", "check-db"]).command(), Command::CheckDb); }`.
```rust
    /// Check the database for corruption and broken references and print row
    /// counts. Never creates or migrates it (used on restored copies).
    CheckDb,
```
In `apps/web/src/main.rs`, add the match arm `Command::CheckDb => commands::check_db(config).await,`.

- [ ] **Step 8: Run all tests, regenerate types, commit**

```bash
SQLX_OFFLINE=true cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings && cargo fmt --check
make types && git diff --stat apps/frontend/src/generated   # Health.ts now has node_id and version
cd apps/frontend && npm run typecheck && npm test && cd ../..
git add -A && git commit -qm "feat: NODE_ID, node id and version in /api/health, and the check-db command"
```

---
### Task 5: Worker crate, its settings, the synced-table list and the Postgres schema

Spec §6.2, §6.9, §9, §10 (test Postgres). This task also adds the contract test from §12.

**Files:**
- Create:
  - `apps/sync-worker/Cargo.toml`
  - `apps/sync-worker/build.rs` (same idea as `apps/web/build.rs`, watching both `../../migrations/sqlite` and `../../migrations/postgres`, one `cargo:rerun-if-changed` line each)
  - `apps/sync-worker/src/lib.rs`
  - `apps/sync-worker/src/config.rs`
  - `apps/sync-worker/src/tables.rs`
  - `migrations/postgres/20261011000000_mirror.sql`
  - `scripts/test-postgres.sh`
  - `apps/sync-worker/tests/common/mod.rs`
  - `apps/sync-worker/tests/contract.rs`
- Modify: `Cargo.toml` (members), `Makefile` (`test`, `check`)

**Interfaces:**
- Consumes: `library_shared::is_valid_node_id` (Task 4) and `sqlite_setup::open_pool` (Task 2).
- Produces:
  - `sync_worker::SQLITE_MIGRATIONS` and `sync_worker::POSTGRES_MIGRATIONS` (`sqlx::migrate::Migrator` statics).
  - `config::{Config, LogFormat}`. `Config` has the fields `node_id: String`, `database_url: String`, `postgres: PgConnectOptions`, `batch_size: usize`, `metrics_addr: SocketAddr` and `log_format: LogFormat`, plus `Config::from_lookup(get)` and `Config::from_env()`.
  - `tables::{Kind, Column, Table, Value, Row, TABLES, by_name, row_id}`, with `Table::column_list()`, `Kind::pg_type()`, `Value::as_int()` and `Value::as_text()`.
  - Test helpers in `tests/common/mod.rs`:
    - `TestSqlite { dir, url, pool }` with `TestSqlite::new()`;
    - `TestPg { options }` with `new()`, `connect()` and `cleanup()`;
    - `insert_book`, `insert_books`, `rename_book`, `delete_book`, `insert_member`, `insert_loan`.

- [ ] **Step 1: Create the crate manifest and register it**

`apps/sync-worker/Cargo.toml`:
```toml
[package]
name = "sync-worker"
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
name = "sync_worker"
path = "src/lib.rs"

[dependencies]
library-shared.workspace = true
sqlite-setup.workspace = true
# The only crate with the Postgres driver (spec §4). rustls lets POSTGRES_URL use `sslmode=require`.
sqlx = { workspace = true, features = ["postgres", "migrate", "macros", "tls-rustls-ring-webpki"] }
tokio = { version = "1", features = ["full"] }
tokio-util = "0.7"
axum = "0.8"
prometheus-client = "0.25"
anyhow = "1"
thiserror = "2"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
clap = { version = "4", features = ["derive"] }
dotenvy = "0.15"
serde_json.workspace = true

[dev-dependencies]
tempfile = "3"
tower = { version = "0.5", features = ["util"] }

[lints]
workspace = true
```
In root `Cargo.toml`, set `members = ["crates/shared", "crates/sqlite", "apps/web", "apps/sync-worker"]`.

- [ ] **Step 2: Write the Postgres migration**

`migrations/postgres/20261011000000_mirror.sql`:
```sql
-- Postgres keeps a copy of every node's books, members and loans (spec §6.2).
-- Same columns as SQLite, plus `node_id` (which node the row lives on) and
-- `synced_at`. Only the primary key is enforced: the node already enforces its
-- own rules, and re-checking them here would make batch order matter.

CREATE TABLE books (
    node_id        TEXT        NOT NULL,
    id             BIGINT      NOT NULL,
    title          TEXT        NOT NULL,
    author         TEXT        NOT NULL,
    isbn           TEXT,
    published_year BIGINT,
    total_copies   BIGINT      NOT NULL,
    created_at     BIGINT      NOT NULL,  -- Unix seconds, exactly as in SQLite
    updated_at     BIGINT      NOT NULL,
    synced_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

CREATE TABLE members (
    node_id    TEXT        NOT NULL,
    id         BIGINT      NOT NULL,
    name       TEXT        NOT NULL,
    email      TEXT        NOT NULL,
    created_at BIGINT      NOT NULL,
    updated_at BIGINT      NOT NULL,
    synced_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

CREATE TABLE loans (
    node_id     TEXT        NOT NULL,
    id          BIGINT      NOT NULL,
    book_id     BIGINT      NOT NULL,
    member_id   BIGINT      NOT NULL,
    borrowed_at BIGINT      NOT NULL,
    due_at      BIGINT      NOT NULL,
    returned_at BIGINT,
    synced_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (node_id, id)
);

-- Where each node's copy stands (spec §6.4).
CREATE TABLE sync_checkpoints (
    node_id    TEXT        PRIMARY KEY,
    epoch      INTEGER     NOT NULL,  -- increases on every full copy
    last_seq   BIGINT      NOT NULL,  -- highest change_log seq applied here
    last_token BYTEA,                 -- token of that change_log row; NULL when last_seq = 0
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Rows replaced by a full copy (spec §6.5), kept for a person to inspect.
CREATE TABLE sync_archive (
    id          BIGSERIAL   PRIMARY KEY,
    node_id     TEXT        NOT NULL,
    epoch       INTEGER     NOT NULL,  -- the epoch these rows belonged to
    table_name  TEXT        NOT NULL,
    row_data    JSONB       NOT NULL,
    archived_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX sync_archive_node_epoch ON sync_archive (node_id, epoch);
```

- [ ] **Step 3: Write the throwaway test Postgres script and wire it into make**

`scripts/test-postgres.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# Start (or reuse) a throwaway Postgres for the worker tests and print its URL.
# Its data lives in memory (tmpfs): removing the container deletes everything.
set -euo pipefail
NAME=sqlite-production-test-pg
PORT=55432
IMAGE=postgres:18.6

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" -e POSTGRES_PASSWORD=test \
    -p "127.0.0.1:$PORT:5432" --tmpfs /var/lib/postgresql "$IMAGE" >/dev/null
fi

# -h 127.0.0.1: during first start the image runs a temporary server on a unix
# socket only; waiting for TCP skips that phase.
for _ in $(seq 1 120); do
  if docker exec "$NAME" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1; then
    echo "postgres://postgres:test@127.0.0.1:$PORT/postgres"
    exit 0
  fi
  sleep 0.5
done
echo "test Postgres did not become ready" >&2
exit 1
```
In `Makefile`:
- replace the `test` target with the version below;
- in `check`, replace the line `cargo test --workspace` with `$(MAKE) test`.
```make
test: ## Rust tests; starts a throwaway Postgres (Docker) for the worker tests
	TEST_POSTGRES_URL=$$(./scripts/test-postgres.sh) cargo test --workspace
```

- [ ] **Step 4: Write `tables.rs` with its unit tests**

`apps/sync-worker/src/tables.rs`:
```rust
//! The tables copied to Postgres, and the plain row type that carries them.
//!
//! Adding a synced table takes three steps (spec §6.9):
//! 1. a SQLite migration with the table and its three triggers;
//! 2. a Postgres migration with the mirror table;
//! 3. one entry in [`TABLES`].
//!
//! The contract test (`tests/contract.rs`) checks that all three agree.

use anyhow::bail;

/// The two kinds of value these tables hold (SQLite INTEGER or TEXT).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Text,
}

impl Kind {
    /// The Postgres type used for this kind, e.g. in `UNNEST($2::bigint[])`.
    pub fn pg_type(self) -> &'static str {
        match self {
            Kind::Int => "bigint",
            Kind::Text => "text",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
    pub nullable: bool,
}

const fn col(name: &'static str, kind: Kind, nullable: bool) -> Column {
    Column { name, kind, nullable }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Table {
    pub name: &'static str,
    /// In the table's column order. The first column is always `id`.
    pub columns: &'static [Column],
}

impl Table {
    /// `id, title, author, ...`, for SELECT and INSERT column lists.
    pub fn column_list(&self) -> String {
        self.columns.iter().map(|c| c.name).collect::<Vec<_>>().join(", ")
    }
}

/// Every synced table, in the order batches list them.
pub static TABLES: &[Table] = &[
    Table {
        name: "books",
        columns: &[
            col("id", Kind::Int, false),
            col("title", Kind::Text, false),
            col("author", Kind::Text, false),
            col("isbn", Kind::Text, true),
            col("published_year", Kind::Int, true),
            col("total_copies", Kind::Int, false),
            col("created_at", Kind::Int, false),
            col("updated_at", Kind::Int, false),
        ],
    },
    Table {
        name: "members",
        columns: &[
            col("id", Kind::Int, false),
            col("name", Kind::Text, false),
            col("email", Kind::Text, false),
            col("created_at", Kind::Int, false),
            col("updated_at", Kind::Int, false),
        ],
    },
    Table {
        name: "loans",
        columns: &[
            col("id", Kind::Int, false),
            col("book_id", Kind::Int, false),
            col("member_id", Kind::Int, false),
            col("borrowed_at", Kind::Int, false),
            col("due_at", Kind::Int, false),
            col("returned_at", Kind::Int, true),
        ],
    },
];

/// Find a synced table by the name stored in `change_log.table_name`.
pub fn by_name(name: &str) -> Option<&'static Table> {
    TABLES.iter().find(|t| t.name == name)
}

/// One column value. These tables hold only integers, text and NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Text(String),
    Null,
}

impl Value {
    /// The value of an integer column; `None` for NULL.
    pub fn as_int(&self) -> anyhow::Result<Option<i64>> {
        match self {
            Value::Int(v) => Ok(Some(*v)),
            Value::Null => Ok(None),
            Value::Text(_) => bail!("expected an integer, found text"),
        }
    }

    /// The value of a text column; `None` for NULL.
    pub fn as_text(&self) -> anyhow::Result<Option<String>> {
        match self {
            Value::Text(v) => Ok(Some(v.clone())),
            Value::Null => Ok(None),
            Value::Int(_) => bail!("expected text, found an integer"),
        }
    }
}

/// A row's values in its table's column order; `row[0]` is the id.
pub type Row = Vec<Value>;

/// The id of a row: its first column.
pub fn row_id(row: &Row) -> Option<i64> {
    match row.first() {
        Some(Value::Int(id)) => Some(*id),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_table_starts_with_a_non_null_integer_id() {
        for table in TABLES {
            assert_eq!(table.columns[0], col("id", Kind::Int, false), "{}", table.name);
        }
    }

    #[test]
    fn table_and_column_names_are_unique() {
        let mut names: Vec<_> = TABLES.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TABLES.len());
        for table in TABLES {
            let mut columns: Vec<_> = table.columns.iter().map(|c| c.name).collect();
            columns.sort_unstable();
            columns.dedup();
            assert_eq!(columns.len(), table.columns.len(), "{}", table.name);
        }
    }

    #[test]
    fn tables_are_found_by_name() {
        assert_eq!(by_name("loans").map(|t| t.name), Some("loans"));
        assert!(by_name("users").is_none(), "users are never synced (spec A4)");
        assert!(by_name("sessions").is_none());
    }

    #[test]
    fn column_list_is_comma_separated() {
        assert_eq!(
            by_name("members").unwrap().column_list(),
            "id, name, email, created_at, updated_at"
        );
    }

    #[test]
    fn values_convert_and_refuse_the_wrong_kind() {
        assert_eq!(Value::Int(3).as_int().unwrap(), Some(3));
        assert_eq!(Value::Null.as_int().unwrap(), None);
        assert!(Value::Text("x".into()).as_int().is_err());
        assert_eq!(Value::Text("x".into()).as_text().unwrap(), Some("x".into()));
        assert_eq!(Value::Null.as_text().unwrap(), None);
        assert!(Value::Int(1).as_text().is_err());
        assert_eq!(row_id(&vec![Value::Int(7), Value::Null]), Some(7));
        assert_eq!(row_id(&vec![]), None);
    }
}
```

- [ ] **Step 5: Write `config.rs` tests first, then the code**

`apps/sync-worker/src/config.rs`:
```rust
//! Settings from environment variables (spec §9).

use std::fmt;
use std::net::SocketAddr;
use std::str::FromStr;

use anyhow::{Context, bail};
use sqlx::postgres::PgConnectOptions;

/// Largest accepted BATCH_SIZE.
const MAX_BATCH_SIZE: usize = 10_000;

/// Output format of the tracing subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Pretty,
    Json,
}

#[derive(Clone)]
pub struct Config {
    /// This node's name; also its `node_id` in Postgres.
    pub node_id: String,
    /// The node's SQLite file, e.g. `sqlite:///data/library.db`.
    pub database_url: String,
    /// The central Postgres. Never printed: it holds a password.
    pub postgres: PgConnectOptions,
    /// Most change_log rows read per batch.
    pub batch_size: usize,
    /// Address of the /health and /metrics server.
    pub metrics_addr: SocketAddr,
    pub log_format: LogFormat,
}

// Written by hand so the Postgres password can never reach a log line.
impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let postgres = format!(
            "{}@{}:{}/{}",
            self.postgres.get_username(),
            self.postgres.get_host(),
            self.postgres.get_port(),
            self.postgres.get_database().unwrap_or("")
        );
        f.debug_struct("Config")
            .field("node_id", &self.node_id)
            .field("database_url", &self.database_url)
            .field("postgres", &postgres)
            .field("batch_size", &self.batch_size)
            .field("metrics_addr", &self.metrics_addr)
            .field("log_format", &self.log_format)
            .finish()
    }
}

impl Config {
    /// Read the process environment. Fails fast with a message naming the bad variable.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    /// Build a config from any key lookup, so tests need not touch the real environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> anyhow::Result<Self> {
        let node_id = get("NODE_ID").context("NODE_ID is required")?;
        if !library_shared::is_valid_node_id(&node_id) {
            bail!(
                "NODE_ID must be 1-40 characters of a-z, 0-9 and `-`, starting with a letter or digit; got `{node_id}`"
            );
        }

        let postgres_url = get("POSTGRES_URL").context("POSTGRES_URL is required")?;
        // The URL holds the password, so this error must not repeat it.
        let postgres = PgConnectOptions::from_str(&postgres_url)
            .map_err(|_| anyhow::anyhow!("POSTGRES_URL is not a valid Postgres URL"))?
            .application_name(&format!("sync-worker {node_id}"));

        let batch_size: usize = parse_or(&get, "BATCH_SIZE", 500)?;
        if !(1..=MAX_BATCH_SIZE).contains(&batch_size) {
            bail!("BATCH_SIZE must be between 1 and {MAX_BATCH_SIZE}, got {batch_size}");
        }

        let log_format = match get("LOG_FORMAT").as_deref() {
            None | Some("pretty") => LogFormat::Pretty,
            Some("json") => LogFormat::Json,
            Some(other) => bail!("LOG_FORMAT must be `pretty` or `json`, got `{other}`"),
        };

        Ok(Self {
            node_id,
            database_url: get("DATABASE_URL")
                .unwrap_or_else(|| "sqlite://data/library.db".to_owned()),
            postgres,
            batch_size,
            metrics_addr: parse_or(&get, "METRICS_ADDR", SocketAddr::from(([127, 0, 0, 1], 9100)))?,
            log_format,
        })
    }
}

/// Parse `key` if present, otherwise use `default`.
fn parse_or<T>(get: &impl Fn(&str) -> Option<String>, key: &str, default: T) -> anyhow::Result<T>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match get(key) {
        None => Ok(default),
        Some(raw) => raw
            .parse()
            .with_context(|| format!("invalid value for {key}: `{raw}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PG: &str = "postgres://sync:s3cret-pw@db.example:5432/library";

    fn lookup<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |key| pairs.iter().find(|(k, _)| *k == key).map(|(_, v)| (*v).to_owned())
    }

    #[test]
    fn minimal_settings_and_defaults() {
        let config = Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", PG)])).unwrap();
        assert_eq!(config.node_id, "node1");
        assert_eq!(config.database_url, "sqlite://data/library.db");
        assert_eq!(config.batch_size, 500);
        assert_eq!(config.metrics_addr.to_string(), "127.0.0.1:9100");
        assert_eq!(config.log_format, LogFormat::Pretty);
        assert_eq!(config.postgres.get_host(), "db.example");
    }

    #[test]
    fn node_id_and_postgres_url_are_required() {
        let err = Config::from_lookup(lookup(&[("POSTGRES_URL", PG)])).unwrap_err();
        assert!(err.to_string().contains("NODE_ID"), "{err}");
        let err = Config::from_lookup(lookup(&[("NODE_ID", "node1")])).unwrap_err();
        assert!(err.to_string().contains("POSTGRES_URL"), "{err}");
    }

    #[test]
    fn bad_node_ids_are_refused() {
        let long = "a".repeat(41);
        for bad in ["", "Node1", "node_1", "-node", "a/b", "../x", "node1\nx", long.as_str()] {
            let err = Config::from_lookup(lookup(&[("NODE_ID", bad), ("POSTGRES_URL", PG)])).unwrap_err();
            assert!(err.to_string().contains("NODE_ID"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn the_postgres_password_never_appears_in_errors_or_debug_output() {
        let err = Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", "postgres://u:s3cret-pw@host:notaport/db")])).unwrap_err();
        assert!(!format!("{err:#}").contains("s3cret-pw"), "{err:#}");
        let config = Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", PG)])).unwrap();
        assert!(!format!("{config:?}").contains("s3cret-pw"));
    }

    #[test]
    fn batch_size_must_be_in_range() {
        for bad in ["0", "10001", "many"] {
            let err = Config::from_lookup(lookup(&[("NODE_ID", "node1"), ("POSTGRES_URL", PG), ("BATCH_SIZE", bad)])).unwrap_err();
            assert!(err.to_string().contains("BATCH_SIZE"), "{bad}: {err}");
        }
    }
}
```
`apps/sync-worker/src/lib.rs`:
```rust
//! sync-worker: copies one node's SQLite changes to the central Postgres
//! (spec §6). `main.rs` is a thin shell; everything testable lives here.

pub mod config;
pub mod tables;

/// The SQLite migrations, embedded so the worker can check that `library-web`
/// has applied them all before it ships anything (spec §6.7). The worker never
/// runs them: `library-web` is the only program that migrates SQLite.
// apps/sync-worker/build.rs rebuilds this crate when either folder changes.
pub static SQLITE_MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations/sqlite");

/// The Postgres migrations. The worker runs these when it connects.
pub static POSTGRES_MIGRATIONS: sqlx::migrate::Migrator =
    sqlx::migrate!("../../migrations/postgres");
```
Run: `cargo test -p sync-worker --lib`
Expected: all `tables` and `config` tests pass. A test that fails here is a bug in the code above; fix the code, not the test.

- [ ] **Step 6: Write the shared test helpers**

`apps/sync-worker/tests/common/mod.rs`:
```rust
//! Helpers for the worker's integration tests: a real SQLite file in a temp
//! directory and a fresh Postgres database per test.
//!
//! Each file in `tests/` is its own crate and uses only some helpers, hence
//! the `dead_code` allowance; panicking on broken setup is the right
//! behaviour in tests.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, SqlitePool};

/// A node's SQLite database as `library-web` leaves it: file created, migrated.
pub struct TestSqlite {
    /// Kept so the directory lives as long as the database.
    pub dir: tempfile::TempDir,
    pub url: String,
    pub pool: SqlitePool,
}

impl TestSqlite {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let url = format!("sqlite://{}", dir.path().join("library.db").display());
        let pool = sqlite_setup::open_pool(&url, 4, true).await.expect("open SQLite");
        sync_worker::SQLITE_MIGRATIONS.run(&pool).await.expect("migrate SQLite");
        Self { dir, url, pool }
    }
}

/// A fresh, empty database on the test server, one per test. The server comes
/// from TEST_POSTGRES_URL; `make test` starts one in Docker.
pub struct TestPg {
    admin: PgConnectOptions,
    name: String,
    /// Connect options for this test's database.
    pub options: PgConnectOptions,
}

impl TestPg {
    pub async fn new() -> Self {
        let url = std::env::var("TEST_POSTGRES_URL").expect(
            "TEST_POSTGRES_URL is not set. Run `make test` (it starts a throwaway Postgres in Docker), \
             or set it to any Postgres where you may create databases.",
        );
        let admin = PgConnectOptions::from_str(&url).expect("valid TEST_POSTGRES_URL");
        let name = unique_name();
        let mut conn = PgConnection::connect_with(&admin).await.expect("connect to test Postgres");
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();
        let options = admin.clone().database(&name);
        Self { admin, name, options }
    }

    pub async fn connect(&self) -> PgConnection {
        PgConnection::connect_with(&self.options).await.expect("connect to test database")
    }

    /// Drop this test's database. Tests call it last; a failed test leaves
    /// its database behind in the throwaway server, which is harmless.
    pub async fn cleanup(self) {
        let mut conn = PgConnection::connect_with(&self.admin).await.expect("connect to test Postgres");
        sqlx::query(AssertSqlSafe(format!("DROP DATABASE {} WITH (FORCE)", self.name)))
            .execute(&mut conn)
            .await
            .expect("drop test database");
        conn.close().await.ok();
    }
}

fn unique_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!(
        "sw_test_{}_{nanos}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Insert a book like the app would; returns its id.
pub async fn insert_book(pool: &SqlitePool, title: &str) -> i64 {
    sqlx::query(
        "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) \
         VALUES (?, 'Author', NULL, 2001, 1, 0, 0)",
    )
    .bind(title)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

/// Insert `count` books titled `bulk 1`, `bulk 2`... in one transaction.
pub async fn insert_books(pool: &SqlitePool, count: usize) {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for i in 1..=count {
        sqlx::query(
            "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) \
             VALUES (?, 'Author', NULL, 2001, 1, 0, 0)",
        )
        .bind(format!("bulk {i}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

pub async fn rename_book(pool: &SqlitePool, id: i64, title: &str) {
    sqlx::query("UPDATE books SET title = ? WHERE id = ?")
        .bind(title).bind(id).execute(pool).await.unwrap();
}

pub async fn delete_book(pool: &SqlitePool, id: i64) {
    sqlx::query("DELETE FROM books WHERE id = ?").bind(id).execute(pool).await.unwrap();
}

pub async fn insert_member(pool: &SqlitePool, email: &str) -> i64 {
    sqlx::query("INSERT INTO members (name, email, created_at, updated_at) VALUES ('Member', ?, 0, 0)")
        .bind(email).execute(pool).await.unwrap().last_insert_rowid()
}

pub async fn insert_loan(pool: &SqlitePool, book_id: i64, member_id: i64) -> i64 {
    sqlx::query("INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES (?, ?, 0, 100, NULL)")
        .bind(book_id).bind(member_id).execute(pool).await.unwrap().last_insert_rowid()
}
```

- [ ] **Step 7: Write the contract test**

`apps/sync-worker/tests/contract.rs`:
```rust
//! The contract between the SQLite schema, the Postgres schema and the
//! worker's table list (spec §5 S11, §6.9). If someone changes a synced table
//! in one place only, this test names the table.
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestPg, TestSqlite};
use sqlx::{AssertSqlSafe, Row};
use sync_worker::tables::{self, Kind};

#[tokio::test]
async fn synced_tables_match_in_sqlite_postgres_and_the_table_list() {
    let sqlite = TestSqlite::new().await;
    let pg = TestPg::new().await;
    let mut conn = pg.connect().await;
    sync_worker::POSTGRES_MIGRATIONS.run(&mut conn).await.unwrap();

    for table in tables::TABLES {
        // SQLite columns: (name, declared type, nullable). An INTEGER PRIMARY
        // KEY reports notnull = 0 but can never be NULL.
        let rows = sqlx::query(AssertSqlSafe(format!("PRAGMA table_info({})", table.name)))
            .fetch_all(&sqlite.pool)
            .await
            .unwrap();
        let in_sqlite: Vec<(String, String, bool)> = rows
            .iter()
            .map(|r| {
                let notnull: i64 = r.get("notnull");
                let pk: i64 = r.get("pk");
                (r.get("name"), r.get("type"), notnull == 0 && pk == 0)
            })
            .collect();
        let expected: Vec<(String, String, bool)> = table
            .columns
            .iter()
            .map(|c| {
                let ty = match c.kind {
                    Kind::Int => "INTEGER",
                    Kind::Text => "TEXT",
                };
                (c.name.to_owned(), ty.to_owned(), c.nullable)
            })
            .collect();
        assert_eq!(in_sqlite, expected, "SQLite table `{}` differs from tables.rs", table.name);

        let in_postgres: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text, is_nullable::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(table.name)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        let mut expected: Vec<(String, String, String)> = vec![("node_id".into(), "text".into(), "NO".into())];
        expected.extend(table.columns.iter().map(|c| {
            let nullable = if c.nullable { "YES" } else { "NO" };
            (c.name.to_owned(), c.kind.pg_type().to_owned(), nullable.to_owned())
        }));
        expected.push(("synced_at".into(), "timestamp with time zone".into(), "NO".into()));
        assert_eq!(in_postgres, expected, "Postgres table `{}` differs from tables.rs", table.name);

        let triggers: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ? ORDER BY name",
        )
        .bind(table.name)
        .fetch_all(&sqlite.pool)
        .await
        .unwrap();
        let expected: Vec<String> = ["delete", "insert", "update"]
            .iter()
            .map(|op| format!("{}_logged_{op}", table.name))
            .collect();
        assert_eq!(triggers, expected, "change-log triggers on `{}`", table.name);
    }

    // Tables that are not synced must not fill the change log.
    let other_triggers: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name NOT IN ('books', 'members', 'loans')",
    )
    .fetch_all(&sqlite.pool)
    .await
    .unwrap();
    assert!(other_triggers.is_empty(), "{other_triggers:?}");

    drop(conn);
    pg.cleanup().await;
}
```

- [ ] **Step 8: Run the tests**

```bash
export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh)
cargo test -p sync-worker
```
Expected: the unit tests and `synced_tables_match_in_sqlite_postgres_and_the_table_list` pass.

Then check the failure message. Temporarily change `published_year` to `Kind::Text` in `tables.rs`, run the contract test, and confirm it fails naming `books`. Then revert the change.

- [ ] **Step 9: Run the full gate and commit**

```bash
make check
git add -A && git commit -qm "feat: sync-worker crate with its settings, synced-table list and Postgres schema"
```

---

### Task 6: SQLite side of the worker: batches, snapshots, pruning

Spec §6.3 (read and prune), §6.4 (checkpoint check), §6.5 (snapshot). In the snapshot, `seq`/`token` come from the newest change_log row, or `(0, None)` when the log is empty. This matches the spec text updated with this plan.

**Files:**
- Create: `apps/sync-worker/src/changes.rs`
- Modify: `apps/sync-worker/src/lib.rs` (add `pub mod changes;`)
- Test: `apps/sync-worker/tests/changes.rs`

**Interfaces:**
- Consumes: `tables::*` (Task 5).
- Produces, used by Tasks 7–9:
  - Types:
    - `pub struct Checkpoint { pub epoch: i32, pub last_seq: i64, pub last_token: Option<Vec<u8>> }`
    - `pub struct Change { pub seq: i64, pub table_name: String, pub row_id: i64, pub token: Vec<u8> }`
    - `pub struct TableChanges { pub table: &'static Table, pub upserts: Vec<Row>, pub deletes: Vec<i64> }`
    - `pub struct Batch { pub changes: usize, pub last_seq: i64, pub last_token: Vec<u8>, pub tables: Vec<TableChanges> }` with `fn rows(&self) -> usize`
    - `pub struct Snapshot { pub seq: i64, pub token: Option<Vec<u8>>, pub tables: Vec<TableChanges> }`
    - `pub enum Read { Invalid, Empty, Batch(Batch) }`
    - `pub struct Pending { pub count: i64, pub oldest_ms: Option<i64> }`
  - Functions:
    - `pub async fn read_batch(pool: &SqlitePool, checkpoint: &Checkpoint, limit: usize) -> anyhow::Result<Read>`
    - `pub async fn read_snapshot(pool: &SqlitePool) -> anyhow::Result<Snapshot>`
    - `pub async fn read_all<'e>(executor: impl SqliteExecutor<'e>, table: &Table) -> anyhow::Result<Vec<Row>>`
    - `pub async fn prune(pool: &SqlitePool, below_seq: i64) -> anyhow::Result<u64>`
    - `pub async fn pending(pool: &SqlitePool, after_seq: i64) -> anyhow::Result<Pending>`
    - `pub fn coalesce(changes: &[Change]) -> anyhow::Result<Vec<(&'static Table, BTreeSet<i64>)>>`

- [ ] **Step 1: Write the failing tests**

`apps/sync-worker/tests/changes.rs`:
```rust
//! The SQLite side of the worker (spec §6.3–6.5), on a real database file.
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestSqlite, delete_book, insert_book, insert_books, insert_loan, insert_member, rename_book};
use sync_worker::changes::{self, Batch, Checkpoint, Read};
use sync_worker::tables::{self, Value};

const START: Checkpoint = Checkpoint { epoch: 1, last_seq: 0, last_token: None };

fn batch(read: Read) -> Batch {
    match read {
        Read::Batch(b) => b,
        other => panic!("expected a batch, got {other:?}"),
    }
}

fn after(b: &Batch) -> Checkpoint {
    Checkpoint { epoch: 1, last_seq: b.last_seq, last_token: Some(b.last_token.clone()) }
}

#[tokio::test]
async fn nothing_to_read_on_a_new_database() {
    let db = TestSqlite::new().await;
    assert_eq!(changes::read_batch(&db.pool, &START, 500).await.unwrap(), Read::Empty);
}

#[tokio::test]
async fn an_insert_is_read_with_its_current_values() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "Dune").await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert_eq!(b.changes, 1);
    assert_eq!(b.tables.len(), 1);
    assert_eq!(b.tables[0].table.name, "books");
    assert_eq!(
        b.tables[0].upserts,
        vec![vec![
            Value::Int(id),
            Value::Text("Dune".into()),
            Value::Text("Author".into()),
            Value::Null,
            Value::Int(2001),
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ]]
    );
    assert!(b.tables[0].deletes.is_empty());
}

#[tokio::test]
async fn many_changes_to_one_row_become_one_row() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "v0").await;
    for i in 1..=100 {
        rename_book(&db.pool, id, &format!("v{i}")).await;
    }
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert_eq!(b.changes, 101);
    assert_eq!(b.rows(), 1);
    assert_eq!(b.tables[0].upserts[0][1], Value::Text("v100".into()));
}

#[tokio::test]
async fn a_row_that_is_gone_is_read_as_a_delete() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "brief").await;
    delete_book(&db.pool, id).await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert!(b.tables[0].upserts.is_empty());
    assert_eq!(b.tables[0].deletes, vec![id]);
}

#[tokio::test]
async fn tables_come_in_the_table_list_order() {
    let db = TestSqlite::new().await;
    let book = insert_book(&db.pool, "b").await;
    let member = insert_member(&db.pool, "m@example.com").await;
    insert_loan(&db.pool, book, member).await;
    rename_book(&db.pool, book, "b2").await; // books changes again after loans
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let names: Vec<_> = b.tables.iter().map(|t| t.table.name).collect();
    assert_eq!(names, ["books", "members", "loans"]);
}

#[tokio::test]
async fn batches_stop_at_the_limit_and_continue_without_gaps() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 1203).await;
    let first = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let second = batch(changes::read_batch(&db.pool, &after(&first), 500).await.unwrap());
    let third = batch(changes::read_batch(&db.pool, &after(&second), 500).await.unwrap());
    assert_eq!((first.changes, second.changes, third.changes), (500, 500, 203));
    assert_eq!(changes::read_batch(&db.pool, &after(&third), 500).await.unwrap(), Read::Empty);
    let ids: Vec<i64> = [&first, &second, &third]
        .iter()
        .flat_map(|b| b.tables[0].upserts.iter().map(|r| tables::row_id(r).unwrap()))
        .collect();
    assert_eq!(ids, (1..=1203).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_checkpoint_whose_row_is_missing_is_invalid() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "x").await;
    let gone = Checkpoint { epoch: 1, last_seq: 999, last_token: Some(vec![1; 8]) };
    assert_eq!(changes::read_batch(&db.pool, &gone, 500).await.unwrap(), Read::Invalid);
}

#[tokio::test]
async fn a_checkpoint_is_valid_only_with_the_same_token() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "x").await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let wrong = Checkpoint { epoch: 1, last_seq: b.last_seq, last_token: Some(vec![0; 8]) };
    assert_eq!(changes::read_batch(&db.pool, &wrong, 500).await.unwrap(), Read::Invalid);
    let none = Checkpoint { epoch: 1, last_seq: b.last_seq, last_token: None };
    assert_eq!(changes::read_batch(&db.pool, &none, 500).await.unwrap(), Read::Invalid);
    assert_eq!(changes::read_batch(&db.pool, &after(&b), 500).await.unwrap(), Read::Empty);
}

#[tokio::test]
async fn an_unknown_table_is_an_error_never_skipped() {
    let db = TestSqlite::new().await;
    sqlx::query("INSERT INTO change_log (table_name, row_id, op) VALUES ('ghosts', 1, 'insert')")
        .execute(&db.pool)
        .await
        .unwrap();
    let err = changes::read_batch(&db.pool, &START, 500).await.unwrap_err();
    assert!(format!("{err:#}").contains("ghosts"), "{err:#}");
}

#[tokio::test]
async fn a_snapshot_of_a_new_database_is_empty_at_zero() {
    let db = TestSqlite::new().await;
    let s = changes::read_snapshot(&db.pool).await.unwrap();
    assert_eq!((s.seq, s.token), (0, None));
    assert_eq!(s.tables.len(), tables::TABLES.len());
    assert!(s.tables.iter().all(|t| t.upserts.is_empty() && t.deletes.is_empty()));
}

#[tokio::test]
async fn a_snapshot_has_every_row_and_the_newest_log_position() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "a").await;
    insert_book(&db.pool, "b").await;
    insert_member(&db.pool, "m@example.com").await;
    let s = changes::read_snapshot(&db.pool).await.unwrap();
    let (seq, token): (i64, Vec<u8>) =
        sqlx::query_as("SELECT seq, token FROM change_log ORDER BY seq DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!((s.seq, s.token.clone()), (seq, Some(token)));
    assert_eq!(s.tables[0].upserts.len(), 2);
    assert_eq!(s.tables[1].upserts.len(), 1);
    assert_eq!(s.tables[0].upserts, changes::read_all(&db.pool, &tables::TABLES[0]).await.unwrap());
}

#[tokio::test]
async fn pruning_keeps_the_checkpoint_row() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 3).await;
    let newest: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log").fetch_one(&db.pool).await.unwrap();
    assert_eq!(changes::prune(&db.pool, newest).await.unwrap(), 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT seq FROM change_log").fetch_all(&db.pool).await.unwrap();
    assert_eq!(left, vec![newest]);
}

#[tokio::test]
async fn pending_counts_what_is_left_and_its_age() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 3).await;
    let first_ms: i64 = sqlx::query_scalar("SELECT MIN(created_at) FROM change_log").fetch_one(&db.pool).await.unwrap();
    let newest: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log").fetch_one(&db.pool).await.unwrap();
    let all = changes::pending(&db.pool, 0).await.unwrap();
    assert_eq!((all.count, all.oldest_ms), (3, Some(first_ms)));
    let none = changes::pending(&db.pool, newest).await.unwrap();
    assert_eq!((none.count, none.oldest_ms), (0, None));
}
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test -p sync-worker --test changes`
Expected: compile errors, because `sync_worker::changes` is missing.

- [ ] **Step 3: Implement `changes.rs`**

`apps/sync-worker/src/changes.rs`:
```rust
//! The SQLite side of the worker (spec §6.3–6.5): reading the change log,
//! the changed rows and full snapshots, and pruning shipped log rows.
//!
//! Each read runs in one SQLite read transaction, so everything it returns
//! comes from one moment in time, while `library-web` keeps writing (WAL lets
//! readers and the writer work side by side).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row as _, Sqlite, SqliteExecutor, SqlitePool, Transaction};

use crate::tables::{self, Kind, Row, Table, Value};

/// Where Postgres's copy of a node stands: one row of `sync_checkpoints`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Increases by one on every full copy.
    pub epoch: i32,
    /// Highest `change_log.seq` already applied to Postgres; 0 before any.
    pub last_seq: i64,
    /// `token` of that change_log row; `None` when `last_seq` is 0.
    pub last_token: Option<Vec<u8>>,
}

/// One change_log row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub seq: i64,
    pub table_name: String,
    pub row_id: i64,
    pub token: Vec<u8>,
}

/// What changed in one table: rows to insert or update, and ids to delete.
#[derive(Debug, Clone, PartialEq)]
pub struct TableChanges {
    pub table: &'static Table,
    pub upserts: Vec<Row>,
    pub deletes: Vec<i64>,
}

/// One batch, ready to ship.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// change_log rows read, before coalescing.
    pub changes: usize,
    /// `seq` and `token` of the last change_log row in the batch; they become
    /// the new checkpoint.
    pub last_seq: i64,
    pub last_token: Vec<u8>,
    /// Only tables with changes, in [`tables::TABLES`] order.
    pub tables: Vec<TableChanges>,
}

impl Batch {
    /// Rows Postgres will insert, update or delete.
    pub fn rows(&self) -> usize {
        self.tables.iter().map(|t| t.upserts.len() + t.deletes.len()).sum()
    }
}

/// Every row of every synced table, and the change_log position it matches.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `seq` of the newest change_log row, or 0 if the log is empty.
    pub seq: i64,
    /// Its token; `None` when `seq` is 0.
    pub token: Option<Vec<u8>>,
    /// One entry per synced table; `deletes` is always empty.
    pub tables: Vec<TableChanges>,
}

/// Result of [`read_batch`].
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// The checkpoint does not match this database: it was restored from an
    /// older backup, or Postgres was (spec §6.4). A full copy is needed.
    Invalid,
    /// Nothing new since the checkpoint.
    Empty,
    Batch(Batch),
}

/// Changes not yet in Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub count: i64,
    /// `created_at` (Unix ms) of the oldest one.
    pub oldest_ms: Option<i64>,
}

/// Read the next batch after `checkpoint`, or report that the checkpoint no
/// longer matches this database.
pub async fn read_batch(pool: &SqlitePool, checkpoint: &Checkpoint, limit: usize) -> anyhow::Result<Read> {
    let mut tx = pool.begin().await.context("starting a SQLite read transaction")?;
    if !checkpoint_is_valid(&mut tx, checkpoint).await? {
        return Ok(Read::Invalid);
    }
    let changes = read_changes(&mut tx, checkpoint.last_seq, limit).await?;
    let Some(last) = changes.last() else {
        return Ok(Read::Empty);
    };
    let (last_seq, last_token) = (last.seq, last.token.clone());

    let mut per_table = Vec::new();
    for (table, ids) in coalesce(&changes)? {
        let upserts = read_rows(&mut tx, table, &ids).await?;
        let present: BTreeSet<i64> = upserts.iter().filter_map(tables::row_id).collect();
        let deletes = ids.difference(&present).copied().collect();
        per_table.push(TableChanges { table, upserts, deletes });
    }
    tx.commit().await.context("ending the SQLite read transaction")?;

    Ok(Read::Batch(Batch {
        changes: changes.len(),
        last_seq,
        last_token,
        tables: per_table,
    }))
}

/// Group change_log rows into the distinct row ids of each table, in
/// [`tables::TABLES`] order: 50 changes to one row become one id. An unknown
/// table name is an error, never skipped: it means this worker is older than
/// the schema and must be upgraded.
pub fn coalesce(changes: &[Change]) -> anyhow::Result<Vec<(&'static Table, BTreeSet<i64>)>> {
    let mut by_table: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for change in changes {
        if tables::by_name(&change.table_name).is_none() {
            bail!(
                "change_log seq {} names table `{}`, which this sync-worker does not know; upgrade the worker",
                change.seq,
                change.table_name
            );
        }
        by_table.entry(change.table_name.as_str()).or_default().insert(change.row_id);
    }
    Ok(tables::TABLES
        .iter()
        .filter_map(|table| by_table.remove(table.name).map(|ids| (table, ids)))
        .collect())
}

/// The check from spec §6.4: valid if nothing was shipped yet, or if the row
/// the checkpoint points at is still here with the same token.
async fn checkpoint_is_valid(tx: &mut Transaction<'_, Sqlite>, checkpoint: &Checkpoint) -> anyhow::Result<bool> {
    if checkpoint.last_seq == 0 {
        return Ok(true);
    }
    let token: Option<Vec<u8>> = sqlx::query_scalar("SELECT token FROM change_log WHERE seq = ?")
        .bind(checkpoint.last_seq)
        .fetch_optional(&mut **tx)
        .await
        .context("looking up the checkpoint row")?;
    Ok(matches!((token, &checkpoint.last_token), (Some(here), Some(there)) if &here == there))
}

async fn read_changes(tx: &mut Transaction<'_, Sqlite>, after_seq: i64, limit: usize) -> anyhow::Result<Vec<Change>> {
    let limit = i64::try_from(limit).context("batch size too large")?;
    let rows = sqlx::query("SELECT seq, table_name, row_id, token FROM change_log WHERE seq > ? ORDER BY seq LIMIT ?")
        .bind(after_seq)
        .bind(limit)
        .fetch_all(&mut **tx)
        .await
        .context("reading change_log")?;
    let changes = rows
        .iter()
        .map(|r| -> Result<Change, sqlx::Error> {
            Ok(Change {
                seq: r.try_get(0)?,
                table_name: r.try_get(1)?,
                row_id: r.try_get(2)?,
                token: r.try_get(3)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(changes)
}

/// The current values of the rows with these ids that still exist.
async fn read_rows(tx: &mut Transaction<'_, Sqlite>, table: &Table, ids: &BTreeSet<i64>) -> anyhow::Result<Vec<Row>> {
    let ids_json = serde_json::to_string(&ids.iter().collect::<Vec<_>>())?;
    // Table and column names come from the fixed list in tables.rs, never from input.
    let sql = format!(
        "SELECT {} FROM {} WHERE id IN (SELECT value FROM json_each(?)) ORDER BY id",
        table.column_list(),
        table.name
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(ids_json)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("reading changed {} rows", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

/// Every row of `table`, ordered by id.
pub async fn read_all<'e>(executor: impl SqliteExecutor<'e>, table: &Table) -> anyhow::Result<Vec<Row>> {
    let sql = format!("SELECT {} FROM {} ORDER BY id", table.column_list(), table.name);
    let rows = sqlx::query(AssertSqlSafe(sql))
        .fetch_all(executor)
        .await
        .with_context(|| format!("reading all {} rows", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

fn decode(table: &Table, row: &SqliteRow) -> anyhow::Result<Row> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| -> anyhow::Result<Value> {
            Ok(match column.kind {
                Kind::Int => row.try_get::<Option<i64>, _>(i)?.map_or(Value::Null, Value::Int),
                Kind::Text => row.try_get::<Option<String>, _>(i)?.map_or(Value::Null, Value::Text),
            })
        })
        .collect()
}

/// Every row of every synced table plus the newest change_log position, all
/// from one moment (spec §6.5).
pub async fn read_snapshot(pool: &SqlitePool) -> anyhow::Result<Snapshot> {
    let mut tx = pool.begin().await.context("starting a SQLite read transaction")?;
    let newest: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, token FROM change_log ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut *tx)
            .await
            .context("reading the newest change_log row")?;
    let (seq, token) = match newest {
        Some((seq, token)) => (seq, Some(token)),
        None => (0, None),
    };
    let mut per_table = Vec::new();
    for table in tables::TABLES {
        let upserts = read_all(&mut *tx, table).await?;
        per_table.push(TableChanges { table, upserts, deletes: Vec::new() });
    }
    tx.commit().await.context("ending the SQLite read transaction")?;
    Ok(Snapshot { seq, token, tables: per_table })
}

/// Delete change_log rows below `seq`. The row *at* `seq` stays: it is the
/// checkpoint row that the next cycle's check looks for (spec §6.4).
pub async fn prune(pool: &SqlitePool, below_seq: i64) -> anyhow::Result<u64> {
    let done = sqlx::query("DELETE FROM change_log WHERE seq < ?")
        .bind(below_seq)
        .execute(pool)
        .await
        .context("pruning change_log")?;
    Ok(done.rows_affected())
}

/// How many changes after `after_seq` are waiting, and how old the oldest is.
pub async fn pending(pool: &SqlitePool, after_seq: i64) -> anyhow::Result<Pending> {
    let (count, oldest_ms): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), MIN(created_at) FROM change_log WHERE seq > ?")
            .bind(after_seq)
            .fetch_one(pool)
            .await
            .context("counting pending changes")?;
    Ok(Pending { count, oldest_ms })
}
```
Add `pub mod changes;` to `lib.rs`.

If the compiler rejects `&mut *tx` or `&mut **tx` as an executor, use whichever form it accepts: `&mut *tx` on a `Transaction` value, `&mut **tx` on a `&mut Transaction`. Do not change behaviour.

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p sync-worker --test changes`
Expected: 13 passed.

- [ ] **Step 5: Run the full gate and commit**

```bash
make check
git add -A && git commit -qm "feat(sync-worker): read batches and snapshots from SQLite, prune shipped changes"
```

---

### Task 7: Postgres side of the worker: the lock, applying batches, full copies

Spec §6.2, §6.3 step 2, §6.5 step 2, §6.6.

**Files:**
- Create: `apps/sync-worker/src/mirror.rs`
- Modify: `apps/sync-worker/src/lib.rs` (`pub mod mirror;`), `apps/sync-worker/tests/common/mod.rs` (add `connect_mirror`)
- Test: `apps/sync-worker/tests/mirror.rs`

**Interfaces:**
- Consumes: `changes::{Batch, Snapshot, Checkpoint}` (Task 6) and `tables::*` (Task 5).
- Produces, used by Tasks 8–9:
  - `pub struct Mirror`
  - `Mirror::connect(options: &PgConnectOptions, node_id: &str) -> Result<Mirror, ConnectError>`
  - `Mirror::node_id(&self) -> &str`
  - `Mirror::load_checkpoint(&mut self) -> anyhow::Result<Option<Checkpoint>>`
  - `Mirror::apply_batch(&mut self, batch: &Batch, epoch: i32) -> anyhow::Result<()>`
  - `Mirror::replace_all(&mut self, snapshot: &Snapshot, old_epoch: i32) -> anyhow::Result<ReplaceReport>`
  - `Mirror::read_all(&mut self, table: &Table) -> anyhow::Result<Vec<Row>>`
  - `Mirror::close(self)`
  - `pub enum ConnectError { Locked(String), Other(anyhow::Error) }`
  - `pub struct ReplaceReport { pub epoch: i32, pub archived: u64, pub copied: u64 }`
  - Free functions:
    - `mirror::load_checkpoint(conn: &mut PgConnection, node_id: &str)`
    - `mirror::read_all(conn: &mut PgConnection, node_id: &str, table: &Table)`
    - `mirror::upsert_sql(table: &Table) -> String`

- [ ] **Step 1: Add the lock-tolerant connect helper to the test helpers**

Append to `apps/sync-worker/tests/common/mod.rs`:
```rust
/// Connect a Mirror, retrying for up to 2 s while the node's lock is still
/// held by a connection that is closing (Postgres releases it a moment after
/// the client hangs up).
pub async fn connect_mirror(pg: &TestPg, node_id: &str) -> sync_worker::mirror::Mirror {
    use sync_worker::mirror::{ConnectError, Mirror};
    for _ in 0..40 {
        match Mirror::connect(&pg.options, node_id).await {
            Ok(mirror) => return mirror,
            Err(ConnectError::Locked(_)) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            Err(ConnectError::Other(err)) => panic!("connect failed: {err:#}"),
        }
    }
    panic!("the lock for {node_id} was never released");
}
```

- [ ] **Step 2: Write the failing tests**

`apps/sync-worker/tests/mirror.rs`:
```rust
//! The Postgres side of the worker (spec §6.2, §6.5, §6.6), on a real Postgres.
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestPg, connect_mirror};
use sync_worker::changes::{Batch, Checkpoint, Snapshot, TableChanges};
use sync_worker::mirror::{ConnectError, Mirror, ReplaceReport};
use sync_worker::tables::{self, Row, Table, Value};

fn books() -> &'static Table {
    tables::by_name("books").unwrap()
}

fn loans() -> &'static Table {
    tables::by_name("loans").unwrap()
}

fn book(id: i64, title: &str) -> Row {
    vec![
        Value::Int(id),
        Value::Text(title.into()),
        Value::Text("A".into()),
        Value::Null,
        Value::Null,
        Value::Int(1),
        Value::Int(10),
        Value::Int(20),
    ]
}

fn batch(last_seq: u8, tables: Vec<TableChanges>) -> Batch {
    Batch { changes: 1, last_seq: i64::from(last_seq), last_token: vec![last_seq; 8], tables }
}

fn books_changes(upserts: Vec<Row>, deletes: Vec<i64>) -> TableChanges {
    TableChanges { table: books(), upserts, deletes }
}

#[tokio::test]
async fn connecting_creates_the_schema_and_takes_the_node_lock() {
    let pg = TestPg::new().await;
    let first = Mirror::connect(&pg.options, "node1").await.unwrap();
    let mut conn = pg.connect().await;
    for table in ["books", "members", "loans", "sync_checkpoints", "sync_archive"] {
        let found: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(table)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(found.as_deref(), Some(table), "connect should have created {table}");
    }
    drop(conn);
    match Mirror::connect(&pg.options, "node1").await {
        Err(ConnectError::Locked(node)) => assert_eq!(node, "node1"),
        other => panic!("expected Locked, got {:?}", other.err()),
    }
    let other_node = Mirror::connect(&pg.options, "node2").await.unwrap();
    first.close().await;
    let again = connect_mirror(&pg, "node1").await; // closing released the lock
    other_node.close().await;
    again.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_new_node_has_no_checkpoint() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    assert_eq!(m.load_checkpoint().await.unwrap(), None);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_batch_upserts_deletes_and_moves_the_checkpoint() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    m.apply_batch(&batch(3, vec![books_changes(vec![book(1, "a"), book(2, "b")], vec![])]), 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), vec![book(1, "a"), book(2, "b")]);
    m.apply_batch(&batch(5, vec![books_changes(vec![book(1, "a2")], vec![2])]), 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), vec![book(1, "a2")]);
    assert_eq!(
        m.load_checkpoint().await.unwrap(),
        Some(Checkpoint { epoch: 1, last_seq: 5, last_token: Some(vec![5; 8]) })
    );
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn applying_the_same_batch_twice_changes_nothing() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let b = batch(4, vec![books_changes(vec![book(1, "a")], vec![9])]);
    m.apply_batch(&b, 1).await.unwrap();
    let rows = m.read_all(books()).await.unwrap();
    let checkpoint = m.load_checkpoint().await.unwrap();
    m.apply_batch(&b, 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    assert_eq!(m.load_checkpoint().await.unwrap(), checkpoint);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn nulls_and_unusual_text_arrive_unchanged() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let long = "x".repeat(2000);
    let rows = vec![
        vec![
            Value::Int(1),
            Value::Text("It's \"quoted\" 📚".into()),
            Value::Text("Ünïcode".into()),
            Value::Null,
            Value::Null,
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ],
        // Empty text and zero are values, not NULL.
        vec![
            Value::Int(2),
            Value::Text(long),
            Value::Text(String::new()),
            Value::Text(String::new()),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ],
    ];
    let loan = vec![Value::Int(7), Value::Int(1), Value::Int(2), Value::Int(100), Value::Int(200), Value::Null];
    let b = batch(
        2,
        vec![
            books_changes(rows.clone(), vec![]),
            TableChanges { table: loans(), upserts: vec![loan.clone()], deletes: vec![] },
        ],
    );
    m.apply_batch(&b, 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    assert_eq!(m.read_all(loans()).await.unwrap(), vec![loan]);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn nodes_do_not_see_each_others_rows() {
    let pg = TestPg::new().await;
    let mut one = connect_mirror(&pg, "node1").await;
    let mut two = connect_mirror(&pg, "node2").await;
    one.apply_batch(&batch(1, vec![books_changes(vec![book(1, "one")], vec![])]), 1).await.unwrap();
    two.apply_batch(&batch(1, vec![books_changes(vec![book(1, "two")], vec![])]), 1).await.unwrap();
    assert_eq!(one.read_all(books()).await.unwrap(), vec![book(1, "one")]);
    assert_eq!(two.read_all(books()).await.unwrap(), vec![book(1, "two")]);
    // A full copy of node1 leaves node2 alone.
    let empty = Snapshot { seq: 0, token: None, tables: vec![] };
    one.replace_all(&empty, 1).await.unwrap();
    assert_eq!(two.read_all(books()).await.unwrap(), vec![book(1, "two")]);
    one.close().await;
    two.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_archives_the_old_rows_and_bumps_the_epoch() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    m.apply_batch(&batch(3, vec![books_changes(vec![book(1, "old"), book(2, "gone")], vec![])]), 1).await.unwrap();
    let snapshot = Snapshot { seq: 9, token: Some(vec![9; 8]), tables: vec![books_changes(vec![book(1, "new")], vec![])] };
    let report = m.replace_all(&snapshot, 1).await.unwrap();
    assert_eq!(report, ReplaceReport { epoch: 2, archived: 2, copied: 1 });
    assert_eq!(m.read_all(books()).await.unwrap(), vec![book(1, "new")]);
    assert_eq!(
        m.load_checkpoint().await.unwrap(),
        Some(Checkpoint { epoch: 2, last_seq: 9, last_token: Some(vec![9; 8]) })
    );
    let mut conn = pg.connect().await;
    let archived: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT epoch, table_name, row_data->>'title' FROM sync_archive WHERE node_id = 'node1' ORDER BY 3",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(archived, vec![(1, "books".into(), "gone".into()), (1, "books".into(), "old".into())]);
    drop(conn);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_larger_than_one_insert_chunk() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let rows: Vec<Row> = (1..=2500).map(|i| book(i, &format!("bulk {i}"))).collect();
    let snapshot = Snapshot { seq: 2500, token: Some(vec![7; 8]), tables: vec![books_changes(rows.clone(), vec![])] };
    let report = m.replace_all(&snapshot, 0).await.unwrap();
    assert_eq!(report, ReplaceReport { epoch: 1, archived: 0, copied: 2500 });
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_of_an_empty_node_sits_at_zero() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let empty = Snapshot { seq: 0, token: None, tables: vec![] };
    assert_eq!(m.replace_all(&empty, 0).await.unwrap(), ReplaceReport { epoch: 1, archived: 0, copied: 0 });
    assert_eq!(m.load_checkpoint().await.unwrap(), Some(Checkpoint { epoch: 1, last_seq: 0, last_token: None }));
    m.close().await;
    pg.cleanup().await;
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test -p sync-worker --test mirror`
Expected: compile errors, because `sync_worker::mirror` is missing.

- [ ] **Step 4: Implement `mirror.rs`**

`apps/sync-worker/src/mirror.rs`:
```rust
//! The Postgres side of the worker (spec §6.2, §6.5, §6.6).
//!
//! A [`Mirror`] owns exactly one Postgres connection. That connection holds
//! the node's advisory lock, and every write goes through it. If the
//! connection drops, Postgres releases the lock *and* our writes fail, so a
//! worker can never write without holding the lock.

use anyhow::Context;
use sqlx::postgres::{PgConnectOptions, PgRow};
use sqlx::{AssertSqlSafe, Connection, PgConnection, Postgres, Row as _, Transaction};

use crate::changes::{Batch, Checkpoint, Snapshot};
use crate::tables::{self, Kind, Row, Table, Value};

/// Rows per INSERT during a full copy.
const COPY_CHUNK: usize = 1000;

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Another worker holds this node's lock (spec §6.6).
    #[error("another sync-worker holds the lock for node `{0}`; is NODE_ID used twice?")]
    Locked(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// What a full copy did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceReport {
    /// The new epoch.
    pub epoch: i32,
    /// Rows moved to `sync_archive`.
    pub archived: u64,
    /// Rows copied from the snapshot.
    pub copied: u64,
}

pub struct Mirror {
    conn: PgConnection,
    node_id: String,
}

impl Mirror {
    /// Connect, run the Postgres migrations, and take the node's lock.
    pub async fn connect(options: &PgConnectOptions, node_id: &str) -> Result<Self, ConnectError> {
        let mut conn = PgConnection::connect_with(options).await.context("connecting to Postgres")?;
        // run_direct, not run: `run` goes through `Acquire`, which makes the
        // worker's future fail the `Send` check when it is `tokio::spawn`ed.
        crate::POSTGRES_MIGRATIONS
            .run_direct(None, &mut conn, false)
            .await
            .context("running the Postgres migrations")?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(format!("sync:{node_id}"))
            .fetch_one(&mut conn)
            .await
            .context("taking the node lock")?;
        if !locked {
            conn.close().await.ok();
            return Err(ConnectError::Locked(node_id.to_owned()));
        }
        Ok(Self { conn, node_id: node_id.to_owned() })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub async fn load_checkpoint(&mut self) -> anyhow::Result<Option<Checkpoint>> {
        load_checkpoint(&mut self.conn, &self.node_id).await
    }

    /// Apply one batch and move the checkpoint, in one transaction (spec §6.3).
    pub async fn apply_batch(&mut self, batch: &Batch, epoch: i32) -> anyhow::Result<()> {
        let mut tx = self.conn.begin().await.context("starting a Postgres transaction")?;
        for changes in &batch.tables {
            upsert(&mut tx, &self.node_id, changes.table, &changes.upserts).await?;
            delete(&mut tx, &self.node_id, changes.table, &changes.deletes).await?;
        }
        let checkpoint = Checkpoint {
            epoch,
            last_seq: batch.last_seq,
            last_token: Some(batch.last_token.clone()),
        };
        save_checkpoint(&mut tx, &self.node_id, &checkpoint).await?;
        tx.commit().await.context("committing the batch")?;
        Ok(())
    }

    /// Replace this node's whole copy with `snapshot`, in one transaction
    /// (spec §6.5). Rows already there go to `sync_archive` under `old_epoch`;
    /// the checkpoint gets epoch `old_epoch + 1`.
    pub async fn replace_all(&mut self, snapshot: &Snapshot, old_epoch: i32) -> anyhow::Result<ReplaceReport> {
        let epoch = old_epoch + 1;
        let mut tx = self.conn.begin().await.context("starting a Postgres transaction")?;
        let mut archived = 0;
        for table in tables::TABLES {
            let archive = format!(
                "INSERT INTO sync_archive (node_id, epoch, table_name, row_data) \
                 SELECT node_id, $2, '{name}', to_jsonb(t) FROM {name} t WHERE node_id = $1",
                name = table.name
            );
            archived += sqlx::query(AssertSqlSafe(archive))
                .bind(self.node_id.clone())
                .bind(old_epoch)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("archiving {} rows", table.name))?
                .rows_affected();
            sqlx::query(AssertSqlSafe(format!("DELETE FROM {} WHERE node_id = $1", table.name)))
                .bind(self.node_id.clone())
                .execute(&mut *tx)
                .await
                .with_context(|| format!("clearing {} rows", table.name))?;
        }
        let mut copied = 0;
        for changes in &snapshot.tables {
            for chunk in changes.upserts.chunks(COPY_CHUNK) {
                upsert(&mut tx, &self.node_id, changes.table, chunk).await?;
                copied += chunk.len() as u64;
            }
        }
        let checkpoint = Checkpoint { epoch, last_seq: snapshot.seq, last_token: snapshot.token.clone() };
        save_checkpoint(&mut tx, &self.node_id, &checkpoint).await?;
        tx.commit().await.context("committing the full copy")?;
        Ok(ReplaceReport { epoch, archived, copied })
    }

    /// Every row Postgres holds for this node in `table`, ordered by id.
    pub async fn read_all(&mut self, table: &Table) -> anyhow::Result<Vec<Row>> {
        read_all(&mut self.conn, &self.node_id, table).await
    }

    /// Hang up cleanly, which releases the node's lock.
    pub async fn close(self) {
        self.conn.close().await.ok();
    }
}

pub async fn load_checkpoint(conn: &mut PgConnection, node_id: &str) -> anyhow::Result<Option<Checkpoint>> {
    let row: Option<(i32, i64, Option<Vec<u8>>)> =
        sqlx::query_as("SELECT epoch, last_seq, last_token FROM sync_checkpoints WHERE node_id = $1")
            .bind(node_id)
            .fetch_optional(conn)
            .await
            .context("reading the checkpoint")?;
    Ok(row.map(|(epoch, last_seq, last_token)| Checkpoint { epoch, last_seq, last_token }))
}

async fn save_checkpoint(tx: &mut Transaction<'_, Postgres>, node_id: &str, checkpoint: &Checkpoint) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO sync_checkpoints (node_id, epoch, last_seq, last_token, updated_at) \
         VALUES ($1, $2, $3, $4, now()) \
         ON CONFLICT (node_id) DO UPDATE SET epoch = EXCLUDED.epoch, last_seq = EXCLUDED.last_seq, \
         last_token = EXCLUDED.last_token, updated_at = now()",
    )
    .bind(node_id)
    .bind(checkpoint.epoch)
    .bind(checkpoint.last_seq)
    .bind(checkpoint.last_token.clone())
    .execute(&mut **tx)
    .await
    .context("saving the checkpoint")?;
    Ok(())
}

/// `INSERT ... SELECT $1, * FROM UNNEST(...) ON CONFLICT DO UPDATE`: one
/// statement and one round trip for any number of rows (spec §6.3).
pub fn upsert_sql(table: &Table) -> String {
    let arrays: Vec<String> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("${}::{}[]", i + 2, c.kind.pg_type()))
        .collect();
    let updates: Vec<String> = table
        .columns
        .iter()
        .skip(1)
        .map(|c| format!("{0} = EXCLUDED.{0}", c.name))
        .collect();
    format!(
        "INSERT INTO {name} (node_id, {columns}) SELECT $1::text, * FROM UNNEST({arrays}) \
         ON CONFLICT (node_id, id) DO UPDATE SET {updates}, synced_at = now()",
        name = table.name,
        columns = table.column_list(),
        arrays = arrays.join(", "),
        updates = updates.join(", "),
    )
}

async fn upsert(tx: &mut Transaction<'_, Postgres>, node_id: &str, table: &Table, rows: &[Row]) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // One array per column; NOT NULL columns are enforced by Postgres itself.
    let mut query = sqlx::query(AssertSqlSafe(upsert_sql(table))).bind(node_id.to_owned());
    // A fn, not a closure: a closure cannot tie its returned reference to its argument.
    fn cell<'a>(row: &'a Row, i: usize, table: &Table) -> anyhow::Result<&'a Value> {
        row.get(i)
            .with_context(|| format!("{} row is missing column {}", table.name, table.columns[i].name))
    }
    for (i, column) in table.columns.iter().enumerate() {
        query = match column.kind {
            Kind::Int => query.bind(rows.iter().map(|r| cell(r, i, table)?.as_int()).collect::<anyhow::Result<Vec<_>>>()?),
            Kind::Text => query.bind(rows.iter().map(|r| cell(r, i, table)?.as_text()).collect::<anyhow::Result<Vec<_>>>()?),
        };
    }
    query
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upserting {} {} rows", rows.len(), table.name))?;
    Ok(())
}

async fn delete(tx: &mut Transaction<'_, Postgres>, node_id: &str, table: &Table, ids: &[i64]) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(AssertSqlSafe(format!("DELETE FROM {} WHERE node_id = $1 AND id = ANY($2)", table.name)))
        .bind(node_id.to_owned())
        .bind(ids.to_vec())
        .execute(&mut **tx)
        .await
        .with_context(|| format!("deleting {} {} rows", ids.len(), table.name))?;
    Ok(())
}

pub async fn read_all(conn: &mut PgConnection, node_id: &str, table: &Table) -> anyhow::Result<Vec<Row>> {
    let sql = format!("SELECT {} FROM {} WHERE node_id = $1 ORDER BY id", table.column_list(), table.name);
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(node_id.to_owned())
        .fetch_all(conn)
        .await
        .with_context(|| format!("reading {} from Postgres", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

fn decode(table: &Table, row: &PgRow) -> anyhow::Result<Row> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| -> anyhow::Result<Value> {
            Ok(match column.kind {
                Kind::Int => row.try_get::<Option<i64>, _>(i)?.map_or(Value::Null, Value::Int),
                Kind::Text => row.try_get::<Option<String>, _>(i)?.map_or(Value::Null, Value::Text),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_sql_for_members() {
        assert_eq!(
            upsert_sql(tables::by_name("members").unwrap()),
            "INSERT INTO members (node_id, id, name, email, created_at, updated_at) \
             SELECT $1::text, * FROM UNNEST($2::bigint[], $3::text[], $4::text[], $5::bigint[], $6::bigint[]) \
             ON CONFLICT (node_id, id) DO UPDATE SET name = EXCLUDED.name, email = EXCLUDED.email, \
             created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at, synced_at = now()"
        );
    }
}
```
Add `pub mod mirror;` to `lib.rs`.

- [ ] **Step 5: Run the tests to see them pass**

Run: `TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) cargo test -p sync-worker --test mirror && cargo test -p sync-worker --lib`
Expected: 9 integration tests and the `upsert_sql_for_members` unit test pass.

- [ ] **Step 6: Run the full gate and commit**

```bash
make check
git add -A && git commit -qm "feat(sync-worker): Postgres mirror with node lock, batched upserts and full copies"
```

---

### Task 8: One sync cycle, and the scenarios that prove it

Spec §6.3–6.5 and §11 (restore rows). This is the core of the project. Every test ends by comparing all rows in SQLite and Postgres.

**Files:**
- Create: `apps/sync-worker/src/sync.rs`
- Modify: `apps/sync-worker/src/lib.rs` (`pub mod sync;`), `apps/sync-worker/tests/common/mod.rs` (add `TestSqlite::backup` and `assert_in_sync`)
- Test: `apps/sync-worker/tests/sync.rs`

**Interfaces:**
- Consumes: `changes::{read_batch, read_snapshot, prune, Read, Checkpoint}` and `mirror::Mirror`.
- Produces, used by Task 9:
  - `pub struct Syncer` with:
    - `Syncer::new(sqlite: SqlitePool, mirror: Mirror, batch_size: usize)`
    - `cycle(&mut self) -> anyhow::Result<Outcome>`
    - `checkpoint(&self) -> Option<&Checkpoint>`
    - `into_mirror(self) -> Mirror`
  - `pub enum Outcome { Idle, Shipped { changes: usize, rows: usize, full: bool }, Copied { resync: bool, epoch: i32, archived: u64, copied: u64 } }`, which derives `Debug, Clone, Copy, PartialEq, Eq`.

- [ ] **Step 1: Add the backup and comparison helpers**

Append to `apps/sync-worker/tests/common/mod.rs`:
```rust
impl TestSqlite {
    /// Copy the database as it is right now into a new file (`VACUUM INTO`),
    /// like a backup taken at this moment, and open the copy.
    pub async fn backup(&self) -> TestSqlite {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("library.db");
        sqlx::query("VACUUM INTO ?")
            .bind(path.display().to_string())
            .execute(&self.pool)
            .await
            .expect("VACUUM INTO");
        let url = format!("sqlite://{}", path.display());
        let pool = sqlite_setup::open_pool(&url, 4, false).await.expect("open backup");
        TestSqlite { dir, url, pool }
    }
}

/// Every synced table holds exactly the same rows in SQLite and in Postgres.
pub async fn assert_in_sync(sqlite: &SqlitePool, pg: &TestPg, node_id: &str) {
    let mut conn = pg.connect().await;
    for table in sync_worker::tables::TABLES {
        let local = sync_worker::changes::read_all(sqlite, table).await.unwrap();
        let remote = sync_worker::mirror::read_all(&mut conn, node_id, table).await.unwrap();
        assert_eq!(local, remote, "table `{}` differs between SQLite and Postgres", table.name);
    }
    conn.close().await.ok();
}
```

- [ ] **Step 2: Write the failing tests**

`apps/sync-worker/tests/sync.rs`:
```rust
//! Sync cycles end to end, on a real SQLite file and a real Postgres
//! (spec §6.3–6.5, §11).
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use sqlx::SqlitePool;
use sync_worker::changes::{self, Checkpoint, Read};
use sync_worker::mirror;
use sync_worker::sync::{Outcome, Syncer};

const NODE: &str = "node1";

async fn syncer(pool: &SqlitePool, pg: &TestPg, batch_size: usize) -> Syncer {
    Syncer::new(pool.clone(), connect_mirror(pg, NODE).await, batch_size)
}

/// Run cycles until one is Idle; return the outcomes before it.
async fn drain(s: &mut Syncer) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    loop {
        let outcome = s.cycle().await.unwrap();
        if outcome == Outcome::Idle {
            return outcomes;
        }
        outcomes.push(outcome);
        assert!(outcomes.len() < 100, "never became idle: {outcomes:?}");
    }
}

async fn log_size(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM change_log").fetch_one(pool).await.unwrap()
}

async fn max_seq(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT MAX(seq) FROM change_log").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn the_first_cycle_copies_what_is_already_there() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    for title in ["a", "b", "c"] {
        insert_book(&db.pool, title).await;
    }
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(s.cycle().await.unwrap(), Outcome::Copied { resync: false, epoch: 1, archived: 0, copied: 3 });
    assert_eq!(s.cycle().await.unwrap(), Outcome::Idle);
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_new_empty_node_starts_at_zero_and_then_ships() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(s.cycle().await.unwrap(), Outcome::Copied { resync: false, epoch: 1, archived: 0, copied: 0 });
    assert_eq!(s.checkpoint(), Some(&Checkpoint { epoch: 1, last_seq: 0, last_token: None }));
    insert_book(&db.pool, "first").await;
    assert_eq!(s.cycle().await.unwrap(), Outcome::Shipped { changes: 1, rows: 1, full: false });
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn inserts_updates_and_deletes_reach_postgres() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    let book = insert_book(&db.pool, "b").await;
    let member = insert_member(&db.pool, "m@example.com").await;
    insert_loan(&db.pool, book, member).await;
    rename_book(&db.pool, book, "b renamed").await;
    // Deleting the member cascades to the loan.
    sqlx::query("DELETE FROM members WHERE id = ?").bind(member).execute(&db.pool).await.unwrap();
    insert_member(&db.pool, "n@example.com").await;
    drain(&mut s).await;
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn repeated_updates_ship_as_one_row() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    let id = insert_book(&db.pool, "v0").await;
    s.cycle().await.unwrap();
    for i in 1..=100 {
        rename_book(&db.pool, id, &format!("v{i}")).await;
    }
    assert_eq!(s.cycle().await.unwrap(), Outcome::Shipped { changes: 100, rows: 1, full: false });
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn more_changes_than_a_batch_ship_in_several_batches() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_books(&db.pool, 1203).await;
    assert_eq!(
        drain(&mut s).await,
        vec![
            Outcome::Shipped { changes: 500, rows: 500, full: true },
            Outcome::Shipped { changes: 500, rows: 500, full: true },
            Outcome::Shipped { changes: 203, rows: 203, full: false },
        ]
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    assert_eq!(log_size(&db.pool).await, 1, "only the checkpoint row is kept");
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_crash_between_commit_and_prune_ships_nothing_twice() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    insert_book(&db.pool, "b").await;
    // Ship by hand and "die" before pruning.
    let mut mirror = s.into_mirror();
    let checkpoint = mirror.load_checkpoint().await.unwrap().unwrap();
    let Read::Batch(batch) = changes::read_batch(&db.pool, &checkpoint, 500).await.unwrap() else {
        panic!("expected a batch");
    };
    mirror.apply_batch(&batch, checkpoint.epoch).await.unwrap();
    // A fresh worker picks up from Postgres's checkpoint.
    let mut s = Syncer::new(db.pool.clone(), mirror, 500);
    assert_eq!(s.cycle().await.unwrap(), Outcome::Idle);
    insert_book(&db.pool, "c").await;
    assert_eq!(s.cycle().await.unwrap(), Outcome::Shipped { changes: 1, rows: 1, full: false });
    assert_in_sync(&db.pool, &pg, NODE).await;
    assert_eq!(log_size(&db.pool).await, 1);
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn restoring_an_older_backup_resyncs_and_archives_the_lost_rows() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "kept").await;
    s.cycle().await.unwrap();
    let backup = db.backup().await; // the newest backup S3 has
    insert_book(&db.pool, "lost").await;
    s.cycle().await.unwrap(); // Postgres got "lost", the backup did not
    // The node dies; a replacement is restored from the backup.
    s.into_mirror().close().await;
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert_eq!(
        restored.cycle().await.unwrap(),
        Outcome::Copied { resync: true, epoch: 2, archived: 2, copied: 1 }
    );
    assert_in_sync(&backup.pool, &pg, NODE).await;
    let mut conn = pg.connect().await;
    let archived: Vec<String> = sqlx::query_scalar(
        "SELECT row_data->>'title' FROM sync_archive WHERE node_id = $1 AND epoch = 1 ORDER BY 1",
    )
    .bind(NODE)
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(archived, vec!["kept".to_owned(), "lost".to_owned()]);
    drop(conn);
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn new_writes_that_reuse_sequence_numbers_are_detected() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "kept").await;
    s.cycle().await.unwrap();
    let backup = db.backup().await;
    insert_book(&db.pool, "lost").await;
    s.cycle().await.unwrap();
    let lost_seq = max_seq(&db.pool).await;
    s.into_mirror().close().await;
    // The restored node takes a write before its worker runs.
    insert_book(&backup.pool, "new after restore").await;
    assert_eq!(max_seq(&backup.pool).await, lost_seq, "the restored database reused the number");
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert!(matches!(restored.cycle().await.unwrap(), Outcome::Copied { resync: true, .. }));
    assert_in_sync(&backup.pool, &pg, NODE).await;
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn restoring_a_newer_backup_carries_on_without_a_resync() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "shipped").await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "not shipped yet").await;
    let backup = db.backup().await; // Litestream was ahead of the worker
    s.into_mirror().close().await;
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert_eq!(restored.cycle().await.unwrap(), Outcome::Shipped { changes: 1, rows: 1, full: false });
    assert_in_sync(&backup.pool, &pg, NODE).await;
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_reused_row_id_ends_with_the_new_values() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    let b = insert_book(&db.pool, "b").await;
    s.cycle().await.unwrap();
    // Delete and re-insert in one batch: SQLite hands out the same id again.
    delete_book(&db.pool, b).await;
    assert_eq!(insert_book(&db.pool, "b2").await, b);
    s.cycle().await.unwrap();
    assert_in_sync(&db.pool, &pg, NODE).await;
    // The same across two batches.
    delete_book(&db.pool, b).await;
    s.cycle().await.unwrap();
    assert_eq!(insert_book(&db.pool, "b3").await, b);
    s.cycle().await.unwrap();
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn postgres_restored_to_an_older_point_gets_a_full_copy() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    s.cycle().await.unwrap();
    // Postgres's checkpoint now: the point its backup will go back to.
    let mut conn = pg.connect().await;
    let old = mirror::load_checkpoint(&mut conn, NODE).await.unwrap().unwrap();
    insert_book(&db.pool, "b").await;
    s.cycle().await.unwrap(); // ships "b" and prunes the change_log row `old` names
    s.into_mirror().close().await;
    // Postgres is restored from that backup: old checkpoint, no "b".
    sqlx::query("UPDATE sync_checkpoints SET last_seq = $2, last_token = $3 WHERE node_id = $1")
        .bind(NODE)
        .bind(old.last_seq)
        .bind(old.last_token.clone())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM books WHERE node_id = $1 AND title = 'b'")
        .bind(NODE)
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(s.cycle().await.unwrap(), Outcome::Copied { resync: true, epoch: 2, archived: 1, copied: 2 });
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn an_unknown_table_stops_the_cycle_without_moving_the_checkpoint() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "x").await;
    sqlx::query("INSERT INTO change_log (table_name, row_id, op) VALUES ('ghosts', 1, 'insert')")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut conn = pg.connect().await;
    let before = mirror::load_checkpoint(&mut conn, NODE).await.unwrap();
    let err = s.cycle().await.unwrap_err();
    assert!(format!("{err:#}").contains("ghosts"), "{err:#}");
    assert_eq!(mirror::load_checkpoint(&mut conn, NODE).await.unwrap(), before);
    drop(conn);
    s.into_mirror().close().await;
    pg.cleanup().await;
}
```

- [ ] **Step 3: Run them to see them fail**

Run: `TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) cargo test -p sync-worker --test sync`
Expected: compile errors, because `sync_worker::sync` is missing.

- [ ] **Step 4: Implement `sync.rs`**

`apps/sync-worker/src/sync.rs`:
```rust
//! One sync cycle (spec §6.3–6.5): check the checkpoint, then ship the next
//! batch, or copy the whole node when the checkpoint no longer matches.

use sqlx::SqlitePool;

use crate::changes::{self, Checkpoint, Read};
use crate::mirror::Mirror;

/// What one cycle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing new.
    Idle,
    /// A batch was applied. `full` means it hit the batch size, so more
    /// changes are probably waiting.
    Shipped { changes: usize, rows: usize, full: bool },
    /// The whole node was copied. `resync` is false for the very first copy
    /// (bootstrap) and true when a restore was detected.
    Copied { resync: bool, epoch: i32, archived: u64, copied: u64 },
}

/// Ships one node's changes. Create one per Postgres session: it reads the
/// checkpoint from Postgres on its first cycle, then keeps it in memory,
/// because it is the only writer of that checkpoint (spec §6.6).
pub struct Syncer {
    sqlite: SqlitePool,
    mirror: Mirror,
    batch_size: usize,
    checkpoint: Option<Checkpoint>,
}

impl Syncer {
    pub fn new(sqlite: SqlitePool, mirror: Mirror, batch_size: usize) -> Self {
        Self { sqlite, mirror, batch_size, checkpoint: None }
    }

    /// The checkpoint after the last successful cycle.
    pub fn checkpoint(&self) -> Option<&Checkpoint> {
        self.checkpoint.as_ref()
    }

    /// Hand back the Postgres side, e.g. to close it.
    pub fn into_mirror(self) -> Mirror {
        self.mirror
    }

    pub async fn cycle(&mut self) -> anyhow::Result<Outcome> {
        let checkpoint = match self.checkpoint.clone() {
            Some(checkpoint) => checkpoint,
            None => match self.mirror.load_checkpoint().await? {
                Some(checkpoint) => checkpoint,
                None => return self.full_copy(None).await,
            },
        };
        match changes::read_batch(&self.sqlite, &checkpoint, self.batch_size).await? {
            Read::Invalid => self.full_copy(Some(checkpoint)).await,
            Read::Empty => {
                self.checkpoint = Some(checkpoint);
                Ok(Outcome::Idle)
            }
            Read::Batch(batch) => {
                self.mirror.apply_batch(&batch, checkpoint.epoch).await?;
                self.checkpoint = Some(Checkpoint {
                    epoch: checkpoint.epoch,
                    last_seq: batch.last_seq,
                    last_token: Some(batch.last_token.clone()),
                });
                // Postgres has committed; if pruning fails now, the next
                // successful batch prunes these rows too.
                changes::prune(&self.sqlite, batch.last_seq).await?;
                Ok(Outcome::Shipped {
                    changes: batch.changes,
                    rows: batch.rows(),
                    full: batch.changes >= self.batch_size,
                })
            }
        }
    }

    /// Copy every row, archiving what Postgres had (spec §6.5). `old` is the
    /// checkpoint that failed the check, or `None` on the very first copy.
    async fn full_copy(&mut self, old: Option<Checkpoint>) -> anyhow::Result<Outcome> {
        let snapshot = changes::read_snapshot(&self.sqlite).await?;
        let old_epoch = old.as_ref().map_or(0, |c| c.epoch);
        let report = self.mirror.replace_all(&snapshot, old_epoch).await?;
        self.checkpoint = Some(Checkpoint {
            epoch: report.epoch,
            last_seq: snapshot.seq,
            last_token: snapshot.token.clone(),
        });
        changes::prune(&self.sqlite, snapshot.seq).await?;
        let resync = old.is_some();
        if resync {
            tracing::warn!(
                node = self.mirror.node_id(),
                epoch = report.epoch,
                archived = report.archived,
                copied = report.copied,
                "the checkpoint did not match this database (restored from an older backup?); \
                 copied the whole node and archived the rows Postgres had"
            );
        } else {
            tracing::info!(
                node = self.mirror.node_id(),
                copied = report.copied,
                archived = report.archived,
                "first sync for this node: copied every row"
            );
        }
        Ok(Outcome::Copied { resync, epoch: report.epoch, archived: report.archived, copied: report.copied })
    }
}
```
Add `pub mod sync;` to `lib.rs`.

- [ ] **Step 5: Run the tests to see them pass**

Run: `TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) cargo test -p sync-worker --test sync`
Expected: 12 passed. If `VACUUM INTO ?` is rejected with a bound parameter, inline the path as a quoted SQL string with `'` doubled. Do not change any assertion.

- [ ] **Step 6: Run the full gate and commit**

```bash
make check
git add -A && git commit -qm "feat(sync-worker): sync cycle with restore detection, resync and archive"
```

---

### Task 9: The worker program: loop, retries, metrics, health, CLI

Spec §6.6–6.8, §8.

**Files:**
- Create: `apps/sync-worker/src/{schema,metrics,http,run,status,cli,telemetry,main}.rs`
- Modify: `apps/sync-worker/Cargo.toml` (add the `[[bin]]`), `apps/sync-worker/src/lib.rs`
- Test: `apps/sync-worker/tests/run.rs`, `apps/sync-worker/tests/schema.rs`; unit tests inside `metrics.rs`, `http.rs`, `run.rs`, `status.rs` and `cli.rs`

**Interfaces:**
- Consumes: `Syncer`, `Outcome`, `Mirror`, `ConnectError`, `changes::{pending, Checkpoint, Pending}` and `Config`.
- Produces, used by Tasks 10–12:
  - binary `sync-worker` with the subcommands `run` (the default) and `status`;
  - HTTP `GET /health` (200 or 503) and `GET /metrics` on `METRICS_ADDR`;
  - metric names `sync_pending_changes`, `sync_lag_seconds`, `sync_last_success_timestamp_seconds`, `sync_epoch`, `sync_batches_total`, `sync_rows_shipped_total`, `sync_errors_total`, `sync_resyncs_total`;
  - `run::run(config: &Config, settings: &Settings, metrics: &Metrics, shutdown: &CancellationToken) -> anyhow::Result<()>`;
  - `run::Settings { batch_size, poll_min, poll_max, retry_min, retry_max, lock_retry, wait_step }` with `Settings::production(batch_size)`;
  - `schema::sqlite_schema_ready(pool) -> anyhow::Result<bool>`.

- [ ] **Step 1: Write the schema handshake test and code**

`apps/sync-worker/tests/schema.rs`:
```rust
//! The schema handshake (spec §6.7).
#![allow(clippy::unwrap_used)]

mod common;

use common::TestSqlite;
use sync_worker::schema::sqlite_schema_ready;

#[tokio::test]
async fn ready_only_when_every_known_migration_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("empty.db").display());
    let empty = sqlite_setup::open_pool(&url, 1, true).await.unwrap();
    assert!(!sqlite_schema_ready(&empty).await.unwrap(), "no _sqlx_migrations table yet");

    let db = TestSqlite::new().await;
    assert!(sqlite_schema_ready(&db.pool).await.unwrap());
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(!sqlite_schema_ready(&db.pool).await.unwrap(), "newest migration missing");
}
```
`apps/sync-worker/src/schema.rs`:
```rust
//! Waiting for `library-web` to migrate SQLite before shipping (spec §6.7).
//! During a rolling deploy the new worker can start before the new web app
//! has migrated; reading a column that does not exist yet would fail.

use std::collections::BTreeSet;

use anyhow::Context;
use sqlx::SqlitePool;

/// True once every migration this worker was built with is recorded as
/// applied in `_sqlx_migrations`.
pub async fn sqlite_schema_ready(pool: &SqlitePool) -> anyhow::Result<bool> {
    let has_table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await
    .context("looking for _sqlx_migrations")?;
    if !has_table {
        return Ok(false);
    }
    let applied: BTreeSet<i64> = sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
        .fetch_all(pool)
        .await
        .context("reading _sqlx_migrations")?
        .into_iter()
        .collect();
    Ok(crate::SQLITE_MIGRATIONS
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .all(|m| applied.contains(&m.version)))
}
```
Add `pub mod schema;` to `lib.rs`. Run `TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) cargo test -p sync-worker --test schema`. Expected: 1 passed.

- [ ] **Step 2: Metrics, with unit tests**

`apps/sync-worker/src/metrics.rs`:
```rust
//! The worker's Prometheus metrics (spec §8) and the clock helpers they use.

use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

pub struct Metrics {
    registry: Registry,
    pub pending_changes: Gauge,
    pub lag_seconds: Gauge<f64, AtomicU64>,
    pub last_success_seconds: Gauge<f64, AtomicU64>,
    pub epoch: Gauge,
    // Counters get `_total` appended to their names when exposed.
    pub batches: Counter,
    pub rows_shipped: Counter,
    pub errors: Counter,
    pub resyncs: Counter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let metrics = Self {
            pending_changes: Gauge::default(),
            lag_seconds: Gauge::default(),
            last_success_seconds: Gauge::default(),
            epoch: Gauge::default(),
            batches: Counter::default(),
            rows_shipped: Counter::default(),
            errors: Counter::default(),
            resyncs: Counter::default(),
            registry: Registry::default(),
        };
        registry.register("sync_pending_changes", "Changes in SQLite not yet in Postgres", metrics.pending_changes.clone());
        registry.register("sync_lag_seconds", "Age of the oldest change not yet in Postgres (0 when none)", metrics.lag_seconds.clone());
        registry.register("sync_last_success_timestamp_seconds", "Unix time of the last successful sync cycle", metrics.last_success_seconds.clone());
        registry.register("sync_epoch", "Epoch of this node's copy in Postgres", metrics.epoch.clone());
        registry.register("sync_batches", "Batches applied to Postgres", metrics.batches.clone());
        registry.register("sync_rows_shipped", "Rows inserted, updated or deleted in Postgres", metrics.rows_shipped.clone());
        registry.register("sync_errors", "Failed sync sessions", metrics.errors.clone());
        registry.register("sync_resyncs", "Full copies made because a restore was detected", metrics.resyncs.clone());
        Self { registry, ..metrics }
    }

    /// The metrics in Prometheus text format.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        if let Err(err) = encode(&mut out, &self.registry) {
            tracing::error!(error = ?err, "encoding metrics failed");
        }
        out
    }

    /// A cycle succeeded within `max_age_secs` of `now_secs`.
    pub fn healthy(&self, now_secs: f64, max_age_secs: f64) -> bool {
        let last = self.last_success_seconds.get();
        last > 0.0 && now_secs - last <= max_age_secs
    }
}

/// Milliseconds since the Unix epoch (the unit of `change_log.created_at`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64())
}

/// Age in seconds of the oldest unsent change; 0 when there is none.
pub fn lag_seconds(oldest_ms: Option<i64>, now_ms: i64) -> f64 {
    oldest_ms.map_or(0.0, |oldest| (now_ms - oldest).max(0) as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_is_exposed_by_name() {
        let m = Metrics::new();
        m.batches.inc();
        let text = m.encode();
        for name in [
            "sync_pending_changes",
            "sync_lag_seconds",
            "sync_last_success_timestamp_seconds",
            "sync_epoch",
            "sync_batches_total",
            "sync_rows_shipped_total",
            "sync_errors_total",
            "sync_resyncs_total",
        ] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
    }

    #[test]
    fn healthy_means_a_recent_success() {
        let m = Metrics::new();
        assert!(!m.healthy(1000.0, 60.0), "never succeeded");
        m.last_success_seconds.set(990.0);
        assert!(m.healthy(1000.0, 60.0));
        assert!(!m.healthy(1100.0, 60.0));
    }

    #[test]
    fn lag_is_zero_without_pending_changes() {
        assert_eq!(lag_seconds(None, 5000), 0.0);
        assert_eq!(lag_seconds(Some(3500), 5000), 1.5);
        assert_eq!(lag_seconds(Some(6000), 5000), 0.0, "clock skew never gives a negative lag");
    }
}
```
If prometheus-client 0.25's API differs, keep the metric names and help text but follow the crate's docs. Check the docs with `cargo doc -p prometheus-client --open`, or read the crate source under `~/.cargo/registry/src/*/prometheus-client-0.25*`. Two calls may differ: `Registry::register` and `Gauge<f64, AtomicU64>::set/get`.

- [ ] **Step 3: HTTP server, with unit tests**

`apps/sync-worker/src/http.rs`:
```rust
//! `/health` and `/metrics` (spec §8). Reachable only inside the Docker
//! network: node.yml never publishes this port.

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::metrics::{Metrics, now_secs};

/// Healthy means a cycle succeeded within this many seconds.
const HEALTHY_WITHIN_SECS: f64 = 60.0;

pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_text))
        .with_state(metrics)
}

async fn health(State(metrics): State<Arc<Metrics>>) -> (StatusCode, &'static str) {
    if metrics.healthy(now_secs(), HEALTHY_WITHIN_SECS) {
        (StatusCode::OK, "ok\n")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no successful sync cycle in the last 60 s\n")
    }
}

async fn metrics_text(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/openmetrics-text; version=1.0.0; charset=utf-8")],
        metrics.encode(),
    )
}

/// Serve until `shutdown` is cancelled.
pub async fn serve(listener: TcpListener, metrics: Arc<Metrics>, shutdown: CancellationToken) -> anyhow::Result<()> {
    axum::serve(listener, router(metrics))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("metrics server")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn get(router: Router, path: &str) -> (StatusCode, String) {
        let response = router.oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_is_503_until_a_cycle_succeeds() {
        let metrics = Arc::new(Metrics::new());
        assert_eq!(get(router(metrics.clone()), "/health").await.0, StatusCode::SERVICE_UNAVAILABLE);
        metrics.last_success_seconds.set(now_secs());
        assert_eq!(get(router(metrics), "/health").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_the_registry() {
        let (status, body) = get(router(Arc::new(Metrics::new())), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("sync_lag_seconds"), "{body}");
    }
}
```

- [ ] **Step 4: The run loop, with unit tests for its timing rules**

`apps/sync-worker/src/run.rs`:
```rust
//! The worker's main loop (spec §6.6–6.8): wait for the database and its
//! schema, then run Postgres sessions, retrying with growing waits, until
//! shutdown. A cycle in progress always finishes before shutting down.

use std::time::Duration;

use sqlx::SqlitePool;
use tokio_util::sync::CancellationToken;

use crate::changes::{self, Checkpoint};
use crate::config::Config;
use crate::metrics::{Metrics, lag_seconds, now_ms, now_secs};
use crate::mirror::{ConnectError, Mirror};
use crate::schema::sqlite_schema_ready;
use crate::sync::{Outcome, Syncer};

/// SQLite connections the worker uses.
const SQLITE_CONNECTIONS: u32 = 2;

/// Timing of the loop. Tests use much shorter values than production.
#[derive(Debug, Clone)]
pub struct Settings {
    pub batch_size: usize,
    /// Wait after a batch that had changes; also the shortest idle wait.
    pub poll_min: Duration,
    /// Longest idle wait.
    pub poll_max: Duration,
    /// First wait after a failed session; doubles up to `retry_max`.
    pub retry_min: Duration,
    pub retry_max: Duration,
    /// Wait before trying again for a lock another worker holds.
    pub lock_retry: Duration,
    /// Wait between checks for the database file and its schema.
    pub wait_step: Duration,
}

impl Settings {
    /// The values from spec §6.3 and §6.8.
    pub fn production(batch_size: usize) -> Self {
        Self {
            batch_size,
            poll_min: Duration::from_millis(200),
            poll_max: Duration::from_secs(2),
            retry_min: Duration::from_secs(1),
            retry_max: Duration::from_secs(30),
            lock_retry: Duration::from_secs(30),
            wait_step: Duration::from_secs(1),
        }
    }
}

/// How long to wait before the next cycle (spec §6.3 step 4).
pub fn next_poll(outcome: &Outcome, previous: Duration, settings: &Settings) -> Duration {
    match outcome {
        Outcome::Shipped { full: true, .. } => Duration::ZERO,
        Outcome::Shipped { .. } | Outcome::Copied { .. } => settings.poll_min,
        Outcome::Idle => (previous * 2).clamp(settings.poll_min, settings.poll_max),
    }
}

/// Wait after a failed session: doubles from `retry_min` up to `retry_max`.
pub fn next_retry(previous: Duration, settings: &Settings) -> Duration {
    (previous * 2).clamp(settings.retry_min, settings.retry_max)
}

enum SessionError {
    Locked(String),
    Failed(anyhow::Error),
}

/// Run until `shutdown` is cancelled. Every failure is retried; nothing here
/// gives up, because a worker that exits would stop the backup silently.
pub async fn run(config: &Config, settings: &Settings, metrics: &Metrics, shutdown: &CancellationToken) -> anyhow::Result<()> {
    let Some(sqlite) = wait_for_sqlite(&config.database_url, settings, shutdown).await else {
        return Ok(());
    };
    let mut retry = Duration::ZERO;
    while !shutdown.is_cancelled() {
        if !wait_for_schema(&sqlite, settings, shutdown).await {
            break;
        }
        match session(&sqlite, config, settings, metrics, shutdown, &mut retry).await {
            Ok(()) => break,
            Err(SessionError::Locked(node)) => {
                metrics.errors.inc();
                tracing::error!(node, "another sync-worker holds this node's lock; is NODE_ID used twice? retrying");
                sleep_or_cancel(settings.lock_retry, shutdown).await;
            }
            Err(SessionError::Failed(err)) => {
                metrics.errors.inc();
                retry = next_retry(retry, settings);
                tracing::warn!(error = format!("{err:#}"), retry_in = ?retry, "sync session failed; retrying");
                sleep_or_cancel(retry, shutdown).await;
            }
        }
    }
    sqlite.close().await;
    Ok(())
}

/// One Postgres session: connect, lock, then cycle until shutdown (Ok) or
/// an error (Err). `retry` is reset after every successful cycle.
async fn session(
    sqlite: &SqlitePool,
    config: &Config,
    settings: &Settings,
    metrics: &Metrics,
    shutdown: &CancellationToken,
    retry: &mut Duration,
) -> Result<(), SessionError> {
    let mirror = match Mirror::connect(&config.postgres, &config.node_id).await {
        Ok(mirror) => mirror,
        Err(ConnectError::Locked(node)) => return Err(SessionError::Locked(node)),
        Err(ConnectError::Other(err)) => return Err(SessionError::Failed(err)),
    };
    tracing::info!(node = %config.node_id, "connected to Postgres and holding the node lock");
    let mut syncer = Syncer::new(sqlite.clone(), mirror, settings.batch_size);
    let mut wait = Duration::ZERO;
    loop {
        if shutdown.is_cancelled() {
            syncer.into_mirror().close().await;
            return Ok(());
        }
        let outcome = syncer.cycle().await.map_err(SessionError::Failed)?;
        record(metrics, &outcome, syncer.checkpoint(), sqlite).await;
        *retry = Duration::ZERO;
        wait = next_poll(&outcome, wait, settings);
        sleep_or_cancel(wait, shutdown).await;
    }
}

async fn record(metrics: &Metrics, outcome: &Outcome, checkpoint: Option<&Checkpoint>, sqlite: &SqlitePool) {
    match outcome {
        Outcome::Shipped { rows, .. } => {
            metrics.batches.inc();
            metrics.rows_shipped.inc_by(*rows as u64);
        }
        Outcome::Copied { resync, copied, .. } => {
            metrics.rows_shipped.inc_by(*copied);
            if *resync {
                metrics.resyncs.inc();
            }
        }
        Outcome::Idle => {}
    }
    if let Some(checkpoint) = checkpoint {
        metrics.epoch.set(i64::from(checkpoint.epoch));
        match changes::pending(sqlite, checkpoint.last_seq).await {
            Ok(pending) => {
                metrics.pending_changes.set(pending.count);
                metrics.lag_seconds.set(lag_seconds(pending.oldest_ms, now_ms()));
            }
            Err(err) => tracing::warn!(error = format!("{err:#}"), "could not count pending changes"),
        }
    }
    metrics.last_success_seconds.set(now_secs());
}

/// Open the node's database, waiting while the file does not exist yet
/// (`library-web` creates it on a fresh node). `None` means shutdown.
async fn wait_for_sqlite(url: &str, settings: &Settings, shutdown: &CancellationToken) -> Option<SqlitePool> {
    let mut logged = false;
    loop {
        if shutdown.is_cancelled() {
            return None;
        }
        match sqlite_setup::open_pool(url, SQLITE_CONNECTIONS, false).await {
            Ok(pool) => return Some(pool),
            Err(err) => {
                if !logged {
                    tracing::info!(error = format!("{err:#}"), "waiting for the SQLite database (library-web creates it)");
                    logged = true;
                }
                sleep_or_cancel(settings.wait_step, shutdown).await;
            }
        }
    }
}

/// Wait until `library-web` has applied every SQLite migration this worker
/// knows (spec §6.7). `false` means shutdown.
async fn wait_for_schema(sqlite: &SqlitePool, settings: &Settings, shutdown: &CancellationToken) -> bool {
    let mut logged = false;
    loop {
        if shutdown.is_cancelled() {
            return false;
        }
        match sqlite_schema_ready(sqlite).await {
            Ok(true) => return true,
            Ok(false) if !logged => {
                tracing::info!("waiting for library-web to apply the SQLite migrations");
                logged = true;
            }
            Ok(false) => {}
            Err(err) => tracing::warn!(error = format!("{err:#}"), "checking the SQLite schema failed"),
        }
        sleep_or_cancel(settings.wait_step, shutdown).await;
    }
}

async fn sleep_or_cancel(duration: Duration, shutdown: &CancellationToken) {
    tokio::select! {
        () = tokio::time::sleep(duration) => {}
        () = shutdown.cancelled() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings::production(500)
    }

    #[test]
    fn a_full_batch_polls_again_at_once() {
        let full = Outcome::Shipped { changes: 500, rows: 500, full: true };
        assert_eq!(next_poll(&full, Duration::from_secs(2), &settings()), Duration::ZERO);
    }

    #[test]
    fn a_partial_batch_or_a_copy_waits_the_minimum() {
        let partial = Outcome::Shipped { changes: 3, rows: 3, full: false };
        assert_eq!(next_poll(&partial, Duration::ZERO, &settings()), Duration::from_millis(200));
        let copied = Outcome::Copied { resync: false, epoch: 1, archived: 0, copied: 0 };
        assert_eq!(next_poll(&copied, Duration::from_secs(2), &settings()), Duration::from_millis(200));
    }

    #[test]
    fn idle_waits_double_from_200ms_up_to_2s() {
        let s = settings();
        let mut wait = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..6 {
            wait = next_poll(&Outcome::Idle, wait, &s);
            seen.push(wait.as_millis());
        }
        assert_eq!(seen, vec![200, 400, 800, 1600, 2000, 2000]);
    }

    #[test]
    fn retries_double_from_1s_up_to_30s() {
        let s = settings();
        let mut wait = Duration::ZERO;
        let mut seen = Vec::new();
        for _ in 0..7 {
            wait = next_retry(wait, &s);
            seen.push(wait.as_secs());
        }
        assert_eq!(seen, vec![1, 2, 4, 8, 16, 30, 30]);
    }
}
```

- [ ] **Step 5: The run loop's integration tests**

`apps/sync-worker/tests/run.rs`:
```rust
//! The whole worker loop against a real SQLite file and Postgres (spec §6.6–6.8).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestPg, TestSqlite, insert_book};
use sync_worker::config::{Config, LogFormat};
use sync_worker::metrics::Metrics;
use sync_worker::run::{self, Settings};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const NODE: &str = "node1";
const APP_NAME: &str = "sync-worker under test";

fn fast() -> Settings {
    Settings {
        batch_size: 500,
        poll_min: Duration::from_millis(10),
        poll_max: Duration::from_millis(50),
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        lock_retry: Duration::from_millis(100),
        wait_step: Duration::from_millis(20),
    }
}

fn config(database_url: &str, pg: &TestPg) -> Config {
    Config {
        node_id: NODE.into(),
        database_url: database_url.into(),
        postgres: pg.options.clone().application_name(APP_NAME),
        batch_size: 500,
        metrics_addr: "127.0.0.1:0".parse().unwrap(),
        log_format: LogFormat::Pretty,
    }
}

fn start(config: Config) -> (JoinHandle<anyhow::Result<()>>, Arc<Metrics>, CancellationToken) {
    let metrics = Arc::new(Metrics::new());
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let (metrics, token) = (metrics.clone(), token.clone());
        async move { run::run(&config, &fast(), &metrics, &token).await }
    });
    (task, metrics, token)
}

/// A COUNT on a table the worker has not created yet is 0 (Postgres error
/// 42P01, undefined_table); any other error fails the test.
fn zero_if_no_table(result: Result<i64, sqlx::Error>) -> i64 {
    match result {
        Ok(count) => count,
        Err(sqlx::Error::Database(err)) if err.code().as_deref() == Some("42P01") => 0,
        Err(err) => panic!("querying Postgres: {err}"),
    }
}

async fn stop(task: JoinHandle<anyhow::Result<()>>, token: CancellationToken) {
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run stops soon after shutdown")
        .unwrap()
        .unwrap();
}

/// Wait up to 10 s for a book with `title` to reach Postgres.
async fn wait_for_title(pg: &TestPg, title: &str) {
    let mut conn = pg.connect().await;
    for _ in 0..200 {
        let found = zero_if_no_table(
            sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE node_id = $1 AND title = $2")
                .bind(NODE)
                .bind(title)
                .fetch_one(&mut conn)
                .await,
        );
        if found == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{title}` never reached Postgres");
}

#[tokio::test]
async fn ships_changes_and_stops_cleanly_on_shutdown() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let (task, metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "hello").await;
    wait_for_title(&pg, "hello").await;
    stop(task, token).await;
    assert!(metrics.last_success_seconds.get() > 0.0);
    assert!(metrics.batches.get() + metrics.rows_shipped.get() >= 1);
    pg.cleanup().await;
}

#[tokio::test]
async fn reconnects_after_losing_the_postgres_connection() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let (task, metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "before").await;
    wait_for_title(&pg, "before").await;
    let mut conn = pg.connect().await;
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1 AND datname = current_database()",
    )
    .bind(APP_NAME)
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(killed, vec![true], "exactly one worker connection");
    insert_book(&db.pool, "after").await;
    wait_for_title(&pg, "after").await;
    assert!(metrics.errors.get() >= 1);
    drop(conn);
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn waits_for_the_database_file_to_appear() {
    let pg = TestPg::new().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("library.db").display());
    let (task, _metrics, token) = start(config(&url, &pg));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished());
    assert!(!dir.path().join("library.db").exists(), "the worker never creates the database");
    // Now library-web creates and migrates it.
    let pool = sqlite_setup::open_pool(&url, 2, true).await.unwrap();
    sync_worker::SQLITE_MIGRATIONS.run(&pool).await.unwrap();
    insert_book(&pool, "late").await;
    wait_for_title(&pg, "late").await;
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn waits_until_web_has_applied_every_migration() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    // Pretend the newest migration has not been applied yet.
    sqlx::query("CREATE TABLE saved_migration AS SELECT * FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&db.pool).await.unwrap();
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version IN (SELECT version FROM saved_migration)")
        .execute(&db.pool).await.unwrap();
    let (task, _metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "waiting").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut conn = pg.connect().await;
    let shipped = zero_if_no_table(sqlx::query_scalar("SELECT COUNT(*) FROM books").fetch_one(&mut conn).await);
    assert_eq!(shipped, 0, "nothing ships before the schema is complete");
    // library-web finishes migrating.
    sqlx::query("INSERT INTO _sqlx_migrations SELECT * FROM saved_migration").execute(&db.pool).await.unwrap();
    wait_for_title(&pg, "waiting").await;
    drop(conn);
    stop(task, token).await;
    pg.cleanup().await;
}
```
Add `pub mod http; pub mod metrics; pub mod run;` to `lib.rs`.

Run: `TEST_POSTGRES_URL=$(./scripts/test-postgres.sh) cargo test -p sync-worker --test run && cargo test -p sync-worker --lib`
Expected: 4 integration tests pass, plus the unit tests in `run`, `metrics` and `http`.

- [ ] **Step 6: `status`, CLI, logging and `main.rs`**

`apps/sync-worker/src/status.rs`:
```rust
//! `sync-worker status`: where this node's copy in Postgres stands (RUNBOOK).

use anyhow::Context;
use sqlx::{Connection, PgConnection};

use crate::changes::{self, Checkpoint, Pending};
use crate::config::Config;
use crate::metrics::{lag_seconds, now_ms};
use crate::mirror;

pub async fn status(config: &Config) -> anyhow::Result<String> {
    let sqlite = sqlite_setup::open_pool(&config.database_url, 1, false).await?;
    let mut conn = PgConnection::connect_with(&config.postgres).await.context("connecting to Postgres")?;
    let checkpoint = mirror::load_checkpoint(&mut conn, &config.node_id).await;
    conn.close().await.ok();
    let text = match checkpoint? {
        None => format!("node: {}\nnot synced yet: Postgres has no checkpoint for this node\n", config.node_id),
        Some(checkpoint) => {
            let pending = changes::pending(&sqlite, checkpoint.last_seq).await?;
            render(&config.node_id, &checkpoint, pending, now_ms())
        }
    };
    sqlite.close().await;
    Ok(text)
}

pub fn render(node_id: &str, checkpoint: &Checkpoint, pending: Pending, now_ms: i64) -> String {
    let waiting = match pending.oldest_ms {
        None => "nothing".to_owned(),
        Some(oldest) => format!("{} changes, oldest {:.1} s old", pending.count, lag_seconds(Some(oldest), now_ms)),
    };
    format!(
        "node: {node_id}\nepoch: {}\nlast shipped seq: {}\nwaiting to ship: {waiting}\n",
        checkpoint.epoch, checkpoint.last_seq
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_shows_position_and_backlog() {
        let checkpoint = Checkpoint { epoch: 3, last_seq: 1234, last_token: None };
        assert_eq!(
            render("node1", &checkpoint, Pending { count: 5, oldest_ms: Some(9_000) }, 10_000),
            "node: node1\nepoch: 3\nlast shipped seq: 1234\nwaiting to ship: 5 changes, oldest 1.0 s old\n"
        );
        assert!(render("node1", &checkpoint, Pending { count: 0, oldest_ms: None }, 10_000).contains("waiting to ship: nothing"));
    }
}
```
`apps/sync-worker/src/cli.rs`:
```rust
//! Command-line interface.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "sync-worker", version, about = "Copies this node's SQLite changes to the central Postgres")]
pub struct Cli {
    /// Defaults to `run` when omitted.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the worker.
    Run,
    /// Print where this node's copy in Postgres stands and what is waiting.
    Status,
}

impl Cli {
    pub fn command(self) -> Command {
        self.command.unwrap_or(Command::Run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_means_run() {
        assert_eq!(Cli::parse_from(["sync-worker"]).command(), Command::Run);
    }

    #[test]
    fn status_subcommand() {
        assert_eq!(Cli::parse_from(["sync-worker", "status"]).command(), Command::Status);
    }
}
```
`apps/sync-worker/src/telemetry.rs`: copy `apps/web/src/telemetry.rs` and make three changes:
- import `LogFormat` from `crate::config`;
- set `DEFAULT_FILTER` to `"info,sync_worker=debug"`;
- update the module doc to name the worker.

`apps/sync-worker/src/main.rs`:
```rust
//! Thin entry point: load `.env`, parse the CLI, set up logging, dispatch.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use sync_worker::cli::{Cli, Command};
use sync_worker::config::Config;
use sync_worker::metrics::Metrics;
use sync_worker::run::{self, Settings};
use sync_worker::{http, status, telemetry};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env file is normal (containers use real env vars).
    dotenvy::dotenv().ok();
    let command = Cli::parse().command();
    let config = Config::from_env()?;
    telemetry::init(config.log_format);
    match command {
        Command::Run => run_worker(config).await,
        Command::Status => {
            print!("{}", status::status(&config).await?);
            Ok(())
        }
    }
}

async fn run_worker(config: Config) -> anyhow::Result<()> {
    let metrics = Arc::new(Metrics::new());
    let shutdown = CancellationToken::new();
    tokio::spawn(cancel_on_signal(shutdown.clone()));
    let listener = tokio::net::TcpListener::bind(config.metrics_addr)
        .await
        .with_context(|| format!("binding {}", config.metrics_addr))?;
    tracing::info!(node = %config.node_id, metrics = %config.metrics_addr, "sync-worker started");
    let server = tokio::spawn(http::serve(listener, metrics.clone(), shutdown.clone()));
    run::run(&config, &Settings::production(config.batch_size), &metrics, &shutdown).await?;
    shutdown.cancel();
    server.await.context("metrics server task")??;
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Cancels `shutdown` on Ctrl-C or SIGTERM (what container runtimes send).
async fn cancel_on_signal(shutdown: CancellationToken) {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = ?err, "failed to listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!(error = ?err, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received; finishing the current cycle");
    shutdown.cancel();
}
```
Add to `apps/sync-worker/Cargo.toml`:
```toml
[[bin]]
name = "sync-worker"
path = "src/main.rs"
```
Add `pub mod cli; pub mod status; pub mod telemetry;` to `lib.rs`. Keep the module list sorted.

- [ ] **Step 7: Try the binary by hand**

```bash
export TEST_POSTGRES_URL=$(./scripts/test-postgres.sh)
# A throwaway database file, so the seed never meets an existing dev database.
rm -f data/manual-check.db data/manual-check.db-wal data/manual-check.db-shm
mkdir -p data && DATABASE_URL=sqlite://data/manual-check.db cargo run -q -p library-web -- seed
cargo build -q -p sync-worker   # build first, so the 5 s timeout below only covers running
docker exec sqlite-production-test-pg psql -U postgres -c 'CREATE DATABASE manual_check' 2>/dev/null || true
NODE_ID=dev POSTGRES_URL=postgres://postgres:test@127.0.0.1:55432/manual_check DATABASE_URL=sqlite://data/dev.db \
  timeout -s TERM 5 cargo run -q -p sync-worker; echo "exit: $?"
NODE_ID=dev POSTGRES_URL=postgres://postgres:test@127.0.0.1:55432/manual_check DATABASE_URL=sqlite://data/dev.db \
  cargo run -q -p sync-worker -- status
docker exec sqlite-production-test-pg psql -U postgres -d manual_check -tAc "SELECT count(*) FROM books WHERE node_id = 'dev'"
docker exec sqlite-production-test-pg psql -U postgres -c 'DROP DATABASE manual_check WITH (FORCE)'
```
Expected:
- The log shows "first sync for this node: copied every row" and then "shut down cleanly".
- `exit: 124`. That is the timeout's own code after it sent TERM; the worker exited by itself on TERM, which the log line proves.
- `status` prints `epoch: 1` and `waiting to ship: nothing`.
- The count is 12.

- [ ] **Step 8: Run the full gate and commit**

```bash
make check
git add -A && git commit -qm "feat(sync-worker): run loop with retries, schema wait, metrics, health and status"
```

---
### Task 10: Docker images and the restore gate

Spec §3, §7.1, §7.2.

**Files:**
- Create:
  - `deploy/docker/Dockerfile`
  - `deploy/docker/litestream.Dockerfile`
  - `deploy/docker/litestream/litestream.yml`
  - `deploy/docker/litestream/restore.sh`
  - `.dockerignore`
  - `scripts/test-restore-gate.sh`
- Modify: `Makefile` (`images` target, and the no-Postgres check in `check`)

**Interfaces:**
- Consumes: the `library-web` and `sync-worker` binaries (Tasks 4 and 9).
- Produces, used by Tasks 11–12:
  - images `sqlite-production/web:dev`, `sqlite-production/sync-worker:dev` and `sqlite-production/litestream:dev`;
  - in the Litestream image, `restore.sh` on `PATH` and config at `/etc/litestream.yml`. Its entrypoint is `litestream` and its default command is `replicate -config /etc/litestream.yml`;
  - `/data` owned by uid 10001 in every image.

- [ ] **Step 1: `.dockerignore` and the Rust/web Dockerfile**

`.dockerignore`:
```
# Keep the build context small and the layer cache stable.
.git
.github
.superpowers
.env
**/target
**/node_modules
**/dist
data
docs
```
`deploy/docker/Dockerfile`:
```dockerfile
# syntax=docker/dockerfile:1
#
# Images for the two Rust programs (spec §3). Pick one with --target:
#   docker build -f deploy/docker/Dockerfile --target web -t sqlite-production/web:dev .
#   docker build -f deploy/docker/Dockerfile --target sync-worker -t sqlite-production/sync-worker:dev .
# Both targets share the build stages, so the second build is quick.

# ---- React build -> /app/apps/frontend/dist --------------------------------------
FROM node:22-bookworm-slim AS frontend
WORKDIR /app/apps/frontend
# Dependencies first, so editing source does not reinstall them.
COPY apps/frontend/package.json apps/frontend/package-lock.json ./
RUN npm ci
# src/generated/ (TypeScript types from the shared crate) is committed: no Rust needed here.
COPY apps/frontend/ ./
RUN npm run build

# ---- Rust build -> /out/library-web and /out/sync-worker -------------------------
FROM rust:1.96-bookworm AS rust
WORKDIR /app
# apps/web's compile-time checked SQL reads the committed .sqlx/ data, not a database.
ENV SQLX_OFFLINE=true
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/
COPY apps/web/ apps/web/
COPY apps/sync-worker/ apps/sync-worker/
COPY migrations/ migrations/
# Two separate builds on purpose: built alone, library-web gets only its own
# SQLx features, so the web binary contains no Postgres driver (spec §4).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/app/target \
    cargo build --release --locked -p library-web \
    && cargo build --release --locked -p sync-worker \
    && mkdir /out \
    && cp target/release/library-web target/release/sync-worker /out/

# ---- Runtime base shared by both programs ----------------------------------------
FROM debian:bookworm-slim AS runtime
# curl is only for the health checks. uid 10001 is shared by every container
# that opens the node's database (spec §3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends curl ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /nonexistent --shell /usr/sbin/nologin app \
    && mkdir /data \
    && chown app:app /data

# ---- web ---------------------------------------------------------------------------
FROM runtime AS web
COPY --from=rust /out/library-web /usr/local/bin/library-web
COPY --from=frontend /app/apps/frontend/dist /srv/frontend
# Set COOKIE_SECURE=true when the app is served over HTTPS (node.yml defaults to true).
ENV DATABASE_URL=sqlite:///data/library.db \
    BIND_ADDR=0.0.0.0:4000 \
    FRONTEND_DIR=/srv/frontend \
    COOKIE_SECURE=false \
    LOG_FORMAT=json
USER app
EXPOSE 4000
HEALTHCHECK --interval=10s --timeout=3s --start-period=30s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:4000/api/health || exit 1
# No arguments means `serve`; other subcommands pass through (seed, create-user, check-db).
ENTRYPOINT ["library-web"]

# ---- sync-worker -------------------------------------------------------------------
FROM runtime AS sync-worker
COPY --from=rust /out/sync-worker /usr/local/bin/sync-worker
ENV DATABASE_URL=sqlite:///data/library.db \
    METRICS_ADDR=0.0.0.0:9100 \
    LOG_FORMAT=json
USER app
HEALTHCHECK --interval=10s --timeout=3s --start-period=60s --retries=3 \
    CMD curl --fail --silent http://127.0.0.1:9100/health || exit 1
ENTRYPOINT ["sync-worker"]
```
If `rust:1.96-bookworm` does not exist, use the newest `rust:1.9x-bookworm` tag that `docker pull` finds. Keep an exact minor version.

- [ ] **Step 2: Litestream config, the restore gate, and its image**

`deploy/docker/litestream/litestream.yml`:
```yaml
# Litestream config for one node (spec §7.1). Values come from environment
# variables; deploy/compose/node.env.example documents them. The access keys
# come from LITESTREAM_ACCESS_KEY_ID and LITESTREAM_SECRET_ACCESS_KEY.

# Metrics for Prometheus; reachable only inside the Docker network.
addr: ":9090"

# A full snapshot every day. Snapshots and the changes between them are kept
# for 3 days: that is how far back a point-in-time restore can go.
snapshot:
  interval: 24h
  retention: 72h

dbs:
  - path: /data/library.db
    replica:
      type: s3
      bucket: ${LITESTREAM_BUCKET}
      # One folder per node. Two nodes must never share it (spec S13).
      path: ${NODE_ID}
      # Empty for AWS S3; the Garage address locally. Setting an endpoint also
      # turns on path-style addressing, which Garage needs.
      endpoint: ${LITESTREAM_ENDPOINT}
      region: ${LITESTREAM_REGION}
```
`deploy/docker/litestream/restore.sh`:
```bash
#!/usr/bin/env bash
# The restore gate (spec §7.2). It runs once before anything else on a node.
# Exit 0 lets the node start. Any other exit keeps the app down, which is the
# safe outcome whenever we cannot be sure whether a backup exists.
set -euo pipefail

DB=/data/library.db
CONFIG=/etc/litestream.yml
node="${NODE_ID:-}"

# 1. The node id is the S3 folder: refuse anything odd. Bash's =~ matches the
#    whole string, so a value with a newline cannot sneak past.
if [[ ! "$node" =~ ^[a-z0-9][a-z0-9-]{0,39}$ ]]; then
  echo "restore: NODE_ID '$node' must be 1-40 characters of a-z, 0-9 and '-', starting with a letter or digit" >&2
  exit 1
fi

# 2. The normal redeploy: the database is already on this disk.
if [[ -f "$DB" ]]; then
  echo "restore: database present at $DB; nothing to do"
  exit 0
fi

# 3. Someone deleted only part of the database. SQLite would replay an old WAL
#    into a new file (spec S12), so a person must look first.
for leftover in "$DB-wal" "$DB-shm" "/data/.library.db-litestream"; do
  if [[ -e "$leftover" ]]; then
    echo "restore: $DB is missing but $leftover exists: part of the database was deleted." \
      "Refusing to start; see RUNBOOK 'Database file missing but its WAL is still there'." >&2
    exit 1
  fi
done

# 4. Empty disk: restore the newest backup. A failure (S3 unreachable, bad
#    keys) must never fall through to an empty start.
echo "restore: no database on this disk; restoring the newest backup of node '$node'"
if ! litestream restore -config "$CONFIG" -if-replica-exists -integrity-check quick "$DB"; then
  echo "restore: litestream restore failed (is S3 reachable?). Not starting with an empty database." >&2
  exit 1
fi

# 5. Restored.
if [[ -f "$DB" ]]; then
  echo "restore: restored $DB from the backup"
  exit 0
fi

# 6. No backup exists. Only a brand-new node may start empty, and only when told to.
if [[ "${ALLOW_FRESH_DB:-false}" == "true" ]]; then
  # Create the file in WAL mode now, so Litestream finds a database to watch
  # even before library-web has started.
  sqlite3 "$DB" 'PRAGMA journal_mode = WAL;' >/dev/null
  echo "restore: no backup exists; starting with a new empty database (ALLOW_FRESH_DB=true)"
  exit 0
fi
echo "restore: No database and no backup for node '$node' at ${LITESTREAM_BUCKET:-?}/$node." \
  "If this is a brand-new node, set ALLOW_FRESH_DB=true for its first start." >&2
exit 1
```
`deploy/docker/litestream.Dockerfile`:
```dockerfile
# syntax=docker/dockerfile:1
#
# Litestream plus our config and the restore gate (spec §7). Each node uses it
# for two containers: `restore` (runs restore.sh once) and `litestream`.
#   docker build -f deploy/docker/litestream.Dockerfile -t sqlite-production/litestream:dev .
FROM litestream/litestream:0.5.17
# sqlite3 lets the restore gate create a brand-new database in WAL mode.
# uid 10001 is shared by every container that opens the database (spec §3).
RUN apt-get update \
    && apt-get install -y --no-install-recommends sqlite3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --user-group --home-dir /nonexistent --shell /usr/sbin/nologin app \
    && mkdir /data \
    && chown app:app /data
COPY deploy/docker/litestream/litestream.yml /etc/litestream.yml
COPY --chmod=0755 deploy/docker/litestream/restore.sh /usr/local/bin/restore.sh
USER app
# The image's entrypoint is `litestream`; `replicate` is the long-running mode.
CMD ["replicate", "-config", "/etc/litestream.yml"]
```
Add to `Makefile`, and add `images` to `.PHONY`:
```make
images: ## Build the three images (tag :dev)
	docker build -f deploy/docker/Dockerfile --target web -t sqlite-production/web:dev .
	docker build -f deploy/docker/Dockerfile --target sync-worker -t sqlite-production/sync-worker:dev .
	docker build -f deploy/docker/litestream.Dockerfile -t sqlite-production/litestream:dev .
```
In `check`, after the `SQLX_OFFLINE=true cargo build` line, add:
```make
	@test -z "$$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)" || { echo "library-web must not depend on a Postgres driver"; exit 1; }
```
`cargo tree -i` prints nothing and exits 0 when the crate is not in `library-web`'s tree, so the check tests for empty output, not the exit code.

- [ ] **Step 3: Write the restore gate test**

`scripts/test-restore-gate.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# Tests the restore gate (spec §7.2) in every case that needs no working S3.
# The S3 cases (restore from a backup, no backup at all) run in scripts/e2e.sh.
set -euo pipefail
IMAGE="${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}"
VOLUMES=()
PASSED=0
cleanup() { for v in "${VOLUMES[@]}"; do docker volume rm -f "$v" >/dev/null; done; }
trap cleanup EXIT

# new_volume <var>: create a scratch volume and store its name in <var>.
# Not `var=$(new_volume)`: a subshell would lose the VOLUMES entry, so every
# volume would get the same name and cleanup would miss them.
new_volume() {
  local v="restore-gate-test-$$-${#VOLUMES[@]}"
  docker volume create "$v" >/dev/null
  VOLUMES+=("$v")
  printf -v "$1" '%s' "$v"
}

# Run a shell command as uid 10001 inside a volume.
in_volume() { docker run --rm -v "$1:/data" --entrypoint sh "$IMAGE" -c "$2"; }

# gate <volume> <expected exit code> <text the output must contain> [docker run options...]
# S3 points at a closed port, so any attempt to reach it fails.
gate() {
  local volume=$1 want=$2 text=$3
  shift 3
  local out code=0
  out=$(docker run --rm -v "$volume:/data" \
    -e LITESTREAM_BUCKET=backups -e LITESTREAM_ENDPOINT=http://127.0.0.1:1 -e LITESTREAM_REGION=test \
    -e LITESTREAM_ACCESS_KEY_ID=test -e LITESTREAM_SECRET_ACCESS_KEY=test \
    "$@" --entrypoint restore.sh "$IMAGE" 2>&1) || code=$?
  if [[ "$code" != "$want" || "$out" != *"$text"* ]]; then
    printf 'FAIL (%s): exit %s, want %s; output:\n%s\n' "$*" "$code" "$want" "$out"
    exit 1
  fi
  PASSED=$((PASSED + 1))
}

new_volume empty
for bad in "" "Node1" "node_1" "-node" "a/b" "../x" $'node1\nx' "$(printf 'a%.0s' {1..41})"; do
  gate "$empty" 1 "must be 1-40 characters" -e "NODE_ID=$bad"
done

new_volume present
in_volume "$present" "sqlite3 /data/library.db 'CREATE TABLE t (x)'"
gate "$present" 0 "database present" -e NODE_ID=node1

new_volume leftover
in_volume "$leftover" "touch /data/library.db-wal"
gate "$leftover" 1 "part of the database was deleted" -e NODE_ID=node1

# The most important case: S3 unreachable must never become an empty start,
# not even for a node that is allowed to start fresh.
gate "$empty" 1 "litestream restore failed" -e NODE_ID=node1 -e ALLOW_FRESH_DB=true
gate "$empty" 1 "litestream restore failed" -e NODE_ID=node1
in_volume "$empty" "test ! -e /data/library.db" || { echo "FAIL: a database was created"; exit 1; }

echo "restore gate: $PASSED checks passed"
```

- [ ] **Step 4: Build the images and run the gate test**

```bash
make images
scripts/test-restore-gate.sh
```
Expected: `restore gate: 12 checks passed`. It takes several minutes: in each of the two unreachable-S3 cases, `litestream restore` retries for about 4 minutes before failing. That is the real behaviour, not a hang. If adding `-e AWS_MAX_ATTEMPTS=1` to the `gate` docker run shortens those cases and they still end with "litestream restore failed", keep it, with a comment saying why.

If the two "litestream restore failed" checks fail because `litestream restore -if-replica-exists` exits 0 when S3 is unreachable, do not weaken the test. Fix the gate instead. Add a step before step 4 of `restore.sh` that fails when S3 cannot be reached but succeeds when the prefix is merely empty. Try `litestream ltx -config "$CONFIG" "$DB"`, and confirm its behaviour against both an unreachable endpoint and an empty Garage prefix; Task 11 has Garage running, so finish the check there if needed. Write what you found in a comment in `restore.sh`.

- [ ] **Step 5: Check the images' users and the config**

```bash
docker run --rm --entrypoint id sqlite-production/web:dev            # uid=10001(app)
docker run --rm --entrypoint id sqlite-production/sync-worker:dev    # uid=10001(app)
docker run --rm --entrypoint id sqlite-production/litestream:dev     # uid=10001(app)
docker run --rm sqlite-production/web:dev --help | head -3           # library-web usage
docker run --rm sqlite-production/sync-worker:dev --help | head -3   # sync-worker usage
docker run --rm -e NODE_ID=node1 -e LITESTREAM_BUCKET=b -e LITESTREAM_ENDPOINT=http://x:3900 -e LITESTREAM_REGION=r \
  sqlite-production/litestream:dev databases -config /etc/litestream.yml   # lists /data/library.db with an s3 replica
make check
```
Expected: the comments above show what each command should print, and `make check` passes. `make check` includes the new "no Postgres driver in library-web" check.

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -qm "build: images for web, sync-worker and Litestream with the restore gate"
```

---

### Task 11: Compose files, Garage, Prometheus and the local cluster

Spec §3, §8, §9, §10.

**Files:**
- Create:
  - `deploy/compose/node.yml`
  - `deploy/compose/node.env.example`
  - `deploy/compose/central.yml`
  - `deploy/compose/local/node1.env`, `deploy/compose/local/node2.env`, `deploy/compose/local/node3.env`
  - `deploy/compose/local/node.override.yml`
  - `deploy/garage/garage.toml`
  - `deploy/garage/init.sh`
  - `deploy/monitoring/prometheus.yml`
  - `deploy/monitoring/alerts.yml`
  - `scripts/cluster.sh`
- Modify: `Makefile`

**Interfaces:**
- Consumes: the three images (Task 10).
- Produces, used by Task 12:
  - `scripts/cluster.sh up|down|reset [--yes]|ps|logs <node>|exec <node> <service> <cmd...>`. It honours the env vars `PREFIX`, `SHARED_NETWORK`, `HTTP_PORT_BASE`, `POSTGRES_HOST_PORT` and `PROMETHEUS_HOST_PORT`.
  - Compose projects `${PREFIX}central` and `${PREFIX}node1..3`, with node volumes `${PREFIX}nodeN_data`.
  - Inside the shared network: Postgres at `postgres:5432`, Garage at `garage:3900`, and node aliases `nodeN-litestream:9090` and `nodeN-worker:9100`.

- [ ] **Step 1: The node file and its documented settings**

`deploy/compose/node.yml`:
```yaml
# One node = one VM (spec §3). Copy this file and an env file (start from
# node.env.example) to the VM, then:
#   docker compose -p <node-id> -f node.yml --env-file node.env up -d
# The local cluster runs this same file three times (scripts/cluster.sh).

x-litestream-env: &litestream-env
  NODE_ID: ${NODE_ID:?set NODE_ID}
  LITESTREAM_BUCKET: ${LITESTREAM_BUCKET:?set LITESTREAM_BUCKET}
  LITESTREAM_ENDPOINT: ${LITESTREAM_ENDPOINT:-}
  LITESTREAM_REGION: ${LITESTREAM_REGION:-us-east-1}
  LITESTREAM_ACCESS_KEY_ID: ${LITESTREAM_ACCESS_KEY_ID:?set LITESTREAM_ACCESS_KEY_ID}
  LITESTREAM_SECRET_ACCESS_KEY: ${LITESTREAM_SECRET_ACCESS_KEY:?set LITESTREAM_SECRET_ACCESS_KEY}

services:
  # One-shot: restore the database from S3 if this disk has none (spec §7.2).
  restore:
    image: ${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}
    entrypoint: ["restore.sh"]
    environment:
      <<: *litestream-env
      ALLOW_FRESH_DB: ${ALLOW_FRESH_DB:-false}
    volumes: [data:/data]
    restart: "no"

  # Streams every change to s3://<bucket>/<node-id>/ (spec §7.1).
  litestream:
    image: ${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}
    environment: *litestream-env
    volumes: [data:/data]
    depends_on:
      restore: { condition: service_completed_successfully }
    restart: unless-stopped
    # Time to upload the last changes when stopping.
    stop_grace_period: 30s

  web:
    image: ${WEB_IMAGE:-sqlite-production/web:dev}
    environment:
      NODE_ID: ${NODE_ID:?set NODE_ID}
      COOKIE_SECURE: ${COOKIE_SECURE:-true}
      SESSION_TTL_HOURS: ${SESSION_TTL_HOURS:-168}
      RUST_LOG: ${RUST_LOG:-info}
    ports:
      - "${HTTP_PORT:-4000}:4000"
    volumes: [data:/data]
    # Starting after litestream means stopping before it, so Litestream
    # uploads the web app's last writes before it stops.
    depends_on:
      restore: { condition: service_completed_successfully }
      litestream: { condition: service_started }
    healthcheck:
      test: ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:4000/api/health"]
      interval: 10s
      timeout: 3s
      retries: 3
      start_period: 30s
      start_interval: 1s
    restart: unless-stopped
    stop_grace_period: 30s

  sync-worker:
    image: ${WORKER_IMAGE:-sqlite-production/sync-worker:dev}
    environment:
      NODE_ID: ${NODE_ID:?set NODE_ID}
      # The only container with Postgres credentials (spec §3).
      POSTGRES_URL: ${POSTGRES_URL:?set POSTGRES_URL}
      BATCH_SIZE: ${BATCH_SIZE:-500}
      RUST_LOG: ${RUST_LOG:-info}
    volumes: [data:/data]
    depends_on:
      web: { condition: service_healthy }
    healthcheck:
      test: ["CMD", "curl", "--fail", "--silent", "http://127.0.0.1:9100/health"]
      interval: 10s
      timeout: 3s
      retries: 3
      start_period: 60s
      start_interval: 1s
    restart: unless-stopped
    stop_grace_period: 30s

volumes:
  # A named volume on the VM's local disk. Never NFS or EFS (spec S1).
  data: {}
```
`deploy/compose/node.env.example`:
```bash
# Settings for one node (one VM). Copy to node.env on the VM, fill it in, and
# start the node with:
#   docker compose -p <node-id> -f node.yml --env-file node.env up -d
# Comments must stay on their own lines (Compose reads the rest of a line as the value).

# Unique name of this node: 1-40 characters of a-z, 0-9 and '-'.
# It is also the S3 folder of its backup and its node_id in Postgres.
NODE_ID=node1
# Host port the web app listens on.
HTTP_PORT=4000
# true ONLY for a brand-new node's first start (spec §7.2); false afterwards.
ALLOW_FRESH_DB=false

# Backup destination: any S3-compatible store.
LITESTREAM_BUCKET=
# Leave empty for AWS S3.
LITESTREAM_ENDPOINT=
LITESTREAM_REGION=us-east-1
LITESTREAM_ACCESS_KEY_ID=
LITESTREAM_SECRET_ACCESS_KEY=

# Central Postgres. Only the sync-worker container receives it.
POSTGRES_URL=postgres://user:password@db.example.com:5432/library?sslmode=require
BATCH_SIZE=500

# Web app. Keep COOKIE_SECURE=true whenever the site is served over HTTPS.
COOKIE_SECURE=true
SESSION_TTL_HOURS=168
RUST_LOG=info

# Images: built by `make images`, or pulled from your registry.
WEB_IMAGE=sqlite-production/web:dev
WORKER_IMAGE=sqlite-production/sync-worker:dev
LITESTREAM_IMAGE=sqlite-production/litestream:dev
```
`deploy/compose/local/node1.env`. Create `node2.env` and `node3.env` the same way, changing only `NODE_ID` and `HTTP_PORT` (`node2`/4002, `node3`/4003):
```bash
# LOCAL CLUSTER ONLY. These credentials are committed on purpose; never use
# them anywhere else. scripts/cluster.sh passes ALLOW_FRESH_DB=true on a
# node's very first start; it stays false here.
NODE_ID=node1
HTTP_PORT=4001
ALLOW_FRESH_DB=false
LITESTREAM_BUCKET=litestream
LITESTREAM_ENDPOINT=http://garage:3900
LITESTREAM_REGION=garage
LITESTREAM_ACCESS_KEY_ID=GK1a0c2d4e6f8091a2b3c4d5e6
LITESTREAM_SECRET_ACCESS_KEY=6c6f63616c2d6465762d6f6e6c792d6e6f742d612d7265616c2d736563726574
POSTGRES_URL=postgres://sync:local-dev-only@postgres:5432/library
BATCH_SIZE=500
COOKIE_SECURE=false
SESSION_TTL_HOURS=168
RUST_LOG=info
```
`deploy/compose/local/node.override.yml`:
```yaml
# Local cluster only: join the shared Docker network where Postgres, Garage
# and Prometheus run, under names Prometheus scrapes (node1-litestream, ...).
# On a real VM, node.yml reaches S3 and Postgres over the network instead.
services:
  restore:
    networks: [default, shared]
  litestream:
    networks:
      default: {}
      shared:
        aliases: ["${NODE_ID}-litestream"]
  sync-worker:
    networks:
      default: {}
      shared:
        aliases: ["${NODE_ID}-worker"]

networks:
  shared:
    external: true
    name: ${SHARED_NETWORK:-sqlite-prod}
```

- [ ] **Step 2: Central services, Garage and Prometheus**

`deploy/compose/central.yml`:
```yaml
# Shared services for the local cluster (spec §10): Postgres, Garage (S3
# stand-in) and Prometheus. In production these are managed services; nodes
# only need their addresses. LOCAL ONLY credentials.
services:
  postgres:
    image: postgres:18.6
    environment:
      POSTGRES_USER: sync
      POSTGRES_PASSWORD: local-dev-only
      POSTGRES_DB: library
    ports:
      - "127.0.0.1:${POSTGRES_HOST_PORT:-5435}:5432"
    volumes:
      - pgdata:/var/lib/postgresql
    healthcheck:
      test: ["CMD", "pg_isready", "-h", "127.0.0.1", "-U", "sync", "-d", "library"]
      interval: 2s
      timeout: 3s
      retries: 30
    networks:
      shared:
        aliases: [postgres]
    restart: unless-stopped

  garage:
    image: dxflrs/garage:v2.4.1
    volumes:
      - ../garage/garage.toml:/etc/garage.toml:ro
      - garage-meta:/var/lib/garage/meta
      - garage-data:/var/lib/garage/data
    networks:
      shared:
        aliases: [garage]
    restart: unless-stopped

  prometheus:
    image: prom/prometheus:v3.13.4
    volumes:
      - ../monitoring:/etc/prometheus:ro
    ports:
      - "127.0.0.1:${PROMETHEUS_HOST_PORT:-9090}:9090"
    networks: [shared]
    restart: unless-stopped

volumes:
  pgdata: {}
  garage-meta: {}
  garage-data: {}

networks:
  shared:
    name: ${SHARED_NETWORK:-sqlite-prod}
```
`deploy/garage/garage.toml`:
```toml
# Garage, the local S3 stand-in (spec §10; decision record 0006).
# LOCAL ONLY: the secrets below are committed on purpose. Never reuse them.
metadata_dir = "/var/lib/garage/meta"
data_dir = "/var/lib/garage/data"
db_engine = "lmdb"
replication_factor = 1

rpc_bind_addr = "[::]:3901"
rpc_public_addr = "127.0.0.1:3901"
rpc_secret = "6761726167652d7270632d6c6f63616c2d6465762d6f6e6c792d736563726574"

[s3_api]
s3_region = "garage"
api_bind_addr = "[::]:3900"
root_domain = ".s3.garage.localhost"

[admin]
api_bind_addr = "[::]:3903"
admin_token = "6761726167652d61646d696e2d6c6f63616c2d6465762d6f6e6c792d746f6b6e"
```
`deploy/garage/init.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# Prepare the local Garage: a one-node layout, the backup bucket and the fixed
# dev key the nodes use. Safe to run again. LOCAL ONLY.
set -euo pipefail
cd "$(dirname "$0")/../.."
PREFIX="${PREFIX:-}"
BUCKET=litestream
KEY_ID=GK1a0c2d4e6f8091a2b3c4d5e6
KEY_SECRET=6c6f63616c2d6465762d6f6e6c792d6e6f742d612d7265616c2d736563726574

garage() { docker compose -p "${PREFIX}central" -f deploy/compose/central.yml exec -T garage /garage "$@"; }

for _ in $(seq 1 60); do
  garage status >/dev/null 2>&1 && break
  sleep 1
done
garage status >/dev/null

# A fresh Garage has no layout (version 0): give its one node 1 GB.
if garage layout show | grep -qiE 'current cluster layout version: 0'; then
  node=$(garage node id -q | cut -d@ -f1)
  garage layout assign -z local -c 1G "$node"
  garage layout apply --version 1
fi
garage bucket info "$BUCKET" >/dev/null 2>&1 || garage bucket create "$BUCKET"
garage key info "$KEY_ID" >/dev/null 2>&1 || garage key import --yes -n litestream-dev "$KEY_ID" "$KEY_SECRET"
garage bucket allow --read --write --owner "$BUCKET" --key "$KEY_ID"
echo "garage: bucket '$BUCKET' ready for key $KEY_ID"
```
Check the Garage commands against the real image before relying on them: run `docker run --rm --entrypoint /garage dxflrs/garage:v2.4.1 <command> --help`. Look at the exact `layout show` wording for an empty layout, and adjust the `grep` if needed. If Garage refuses the config because the secrets are readable by everyone, add `GARAGE_ALLOW_WORLD_READABLE_SECRETS: "true"` to the `garage` service environment, with a comment.

`deploy/monitoring/prometheus.yml`:
```yaml
# Scrapes every local node's Litestream and sync-worker (spec §8). On real
# VMs, list each VM's address instead, and keep these ports off the internet.
global:
  scrape_interval: 5s
  evaluation_interval: 5s

rule_files:
  - alerts.yml

scrape_configs:
  - job_name: litestream
    static_configs:
      - targets: [node1-litestream:9090, node2-litestream:9090, node3-litestream:9090]
  - job_name: sync-worker
    static_configs:
      - targets: [node1-worker:9100, node2-worker:9100, node3-worker:9100]
```
`deploy/monitoring/alerts.yml`. Write the four rules below now. Step 5 adds `LitestreamStalled` and `WalTooLarge` once the real Litestream metric names are known:
```yaml
# Alert rules (spec §8). There is no Alertmanager in the local cluster: see
# firing alerts at http://127.0.0.1:9090/alerts. Each rule names the RUNBOOK
# section to follow.
groups:
  - name: sqlite-production
    rules:
      - alert: SyncLagHigh
        expr: sync_lag_seconds > 60
        for: 5m
        annotations:
          summary: "{{ $labels.instance }}: Postgres is {{ $value }}s behind this node"
          runbook: "docs/RUNBOOK.md#postgres-down"
      - alert: SyncResync
        expr: increase(sync_resyncs_total[1h]) > 0
        annotations:
          summary: "{{ $labels.instance }} was restored from an older backup and may have lost writes; check sync_archive"
          runbook: "docs/RUNBOOK.md#after-a-resync"
      - alert: SyncWorkerDown
        expr: up{job="sync-worker"} == 0
        for: 2m
        annotations:
          summary: "{{ $labels.instance }}: sync-worker is down; Postgres is not receiving changes"
          runbook: "docs/RUNBOOK.md#postgres-down"
      - alert: LitestreamDown
        expr: up{job="litestream"} == 0
        for: 2m
        annotations:
          summary: "{{ $labels.instance }}: Litestream is down; the S3 backup has stopped"
          runbook: "docs/RUNBOOK.md#s3-down"
```

- [ ] **Step 3: The cluster script and Makefile targets**

`scripts/cluster.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# The local cluster (spec §10): central services plus three nodes. Each node
# is the same deploy/compose/node.yml a real VM runs.
#   scripts/cluster.sh up | down | reset [--yes] | ps | logs <node> | exec <node> <service> <cmd...>
# PREFIX (default empty) names the compose projects, so scripts/e2e.sh can run
# a second, separate cluster. HTTP_PORT_BASE (default 4000) gives node N the
# port BASE+N.
set -euo pipefail
cd "$(dirname "$0")/.."
PREFIX="${PREFIX:-}"
export SHARED_NETWORK="${SHARED_NETWORK:-${PREFIX}sqlite-prod}"
HTTP_PORT_BASE="${HTTP_PORT_BASE:-4000}"
NODES=(node1 node2 node3)

central() { docker compose -p "${PREFIX}central" -f deploy/compose/central.yml "$@"; }
node() {
  local n=$1
  shift
  HTTP_PORT=$((HTTP_PORT_BASE + ${n#node})) docker compose -p "${PREFIX}${n}" \
    -f deploy/compose/node.yml -f deploy/compose/local/node.override.yml \
    --env-file "deploy/compose/local/${n}.env" "$@"
}

case "${1:-}" in
  up)
    central up -d --wait postgres
    central up -d
    PREFIX="$PREFIX" deploy/garage/init.sh
    for n in "${NODES[@]}"; do
      # A brand-new node needs ALLOW_FRESH_DB=true once (spec §7.2). A node
      # whose volume exists keeps its data, or restores it from S3.
      if docker volume inspect "${PREFIX}${n}_data" >/dev/null 2>&1; then fresh=false; else fresh=true; fi
      ALLOW_FRESH_DB=$fresh node "$n" up -d
    done
    echo "cluster up: nodes on ports $((HTTP_PORT_BASE + 1))-$((HTTP_PORT_BASE + 3)), Prometheus on ${PROMETHEUS_HOST_PORT:-9090}"
    ;;
  down)
    for n in "${NODES[@]}"; do node "$n" down; done
    central down
    ;;
  reset)
    if [[ "${2:-}" != "--yes" ]]; then
      read -r -p "Delete ALL data of the '${PREFIX}' cluster (databases, backups, Postgres)? [y/N] " answer
      [[ "$answer" == y ]] || exit 1
    fi
    for n in "${NODES[@]}"; do node "$n" down -v; done
    central down -v
    ;;
  ps)
    central ps
    for n in "${NODES[@]}"; do node "$n" ps; done
    ;;
  logs)
    node "${2:?usage: cluster.sh logs <node>}" logs -f
    ;;
  exec)
    n=${2:?usage: cluster.sh exec <node> <service> <cmd...>}
    shift 2
    node "$n" exec "$@"
    ;;
  *)
    echo "usage: scripts/cluster.sh up | down | reset [--yes] | ps | logs <node> | exec <node> <service> <cmd...>" >&2
    exit 2
    ;;
esac
```
Add to `Makefile`, and add these names to `.PHONY`:
```make
up: images ## Start Postgres, Garage, Prometheus and nodes 1-3 (ports 4001-4003)
	scripts/cluster.sh up

down: ## Stop the cluster; its data is kept
	scripts/cluster.sh down

reset: ## Stop the cluster and delete all its data (asks first)
	scripts/cluster.sh reset

ps: ## Show the cluster's containers
	scripts/cluster.sh ps

logs: ## Follow one node's logs: make logs NODE=node2
	scripts/cluster.sh logs $(or $(NODE),node1)

psql: ## psql on the cluster's Postgres
	docker compose -p central -f deploy/compose/central.yml exec postgres psql -U sync -d library

seed: ## Sample data on a cluster node: make seed NODE=node1
	scripts/cluster.sh exec $(or $(NODE),node1) web library-web seed

user: ## Staff account on a cluster node: make user NODE=node1 NAME=alice
	scripts/cluster.sh exec $(or $(NODE),node1) web library-web create-user $(or $(NAME),admin)
```

- [ ] **Step 4: Bring the cluster up and check every part**

```bash
make up
scripts/cluster.sh ps                                   # restore exited 0; litestream, web, sync-worker up and healthy
for p in 4001 4002 4003; do curl -s localhost:$p/api/health; echo; done   # node_id node1/node2/node3
docker compose -p node1 -f deploy/compose/node.yml --env-file deploy/compose/local/node1.env logs restore
# expect: "starting with a new empty database (ALLOW_FRESH_DB=true)"
make seed NODE=node1
sleep 3
docker compose -p central -f deploy/compose/central.yml exec -T postgres psql -U sync -d library -tAc \
  "SELECT node_id, count(*) FROM books GROUP BY 1"      # node1|12
docker compose -p node1 -f deploy/compose/node.yml --env-file deploy/compose/local/node1.env logs litestream | tail -20   # no errors
docker compose -p central -f deploy/compose/central.yml exec -T garage /garage bucket info litestream   # objects > 0
curl -s localhost:9090/api/v1/targets | jq '[.data.activeTargets[] | {job: .labels.job, instance: .labels.instance, health}]'   # 6 targets, all "up"
```
Expected: what each comment says.

Then run the two safety checks from spec §7.2:
```bash
# (a) A plain restart keeps the data and restores nothing.
make down && scripts/cluster.sh up
docker compose -p node1 -f deploy/compose/node.yml --env-file deploy/compose/local/node1.env logs restore | grep "database present"
# (b) S3 down while a node with an empty disk starts: it must refuse, even with ALLOW_FRESH_DB=true.
docker compose -p central -f deploy/compose/central.yml stop garage
HTTP_PORT=4003 docker compose -p node3 -f deploy/compose/node.yml -f deploy/compose/local/node.override.yml --env-file deploy/compose/local/node3.env down -v
HTTP_PORT=4003 ALLOW_FRESH_DB=true docker compose -p node3 -f deploy/compose/node.yml -f deploy/compose/local/node.override.yml --env-file deploy/compose/local/node3.env up -d; echo "up exit: $?"
docker compose -p node3 -f deploy/compose/node.yml --env-file deploy/compose/local/node3.env ps -a   # restore exited 1; web not running
docker compose -p central -f deploy/compose/central.yml start garage
HTTP_PORT=4003 docker compose -p node3 -f deploy/compose/node.yml -f deploy/compose/local/node.override.yml --env-file deploy/compose/local/node3.env up -d
docker compose -p node3 -f deploy/compose/node.yml --env-file deploy/compose/local/node3.env logs restore | tail -3   # "restored /data/library.db from the backup"
```
Expected:
- (a) prints "database present".
- (b) `up exit:` is non-zero and restore exited 1. After Garage is back, node3 is restored.

If (b) starts web instead, the gate has the problem described in Task 10 Step 4. Fix `restore.sh` now, and rerun `scripts/test-restore-gate.sh` and this check.

- [ ] **Step 5: Add the two Litestream alert rules from the real metric names**

```bash
curl -s localhost:9090/api/v1/label/__name__/values | jq -r '.data[]' | grep '^litestream_'
```
From that list, add two rules to `alerts.yml`, in the same style as the others, with `summary` and `runbook` annotations:
- **`WalTooLarge`**:
  - use the metric whose name contains `wal` and `size` (bytes);
  - `expr: <that metric> > 256 * 1024 * 1024`, `for: 15m`;
  - runbook `docs/RUNBOOK.md#disk-filling-up`.
- **`LitestreamStalled`**:
  - fires when replication stops while the database keeps changing;
  - `for: 5m`, runbook `docs/RUNBOOK.md#s3-down`;
  - prefer `increase(<successful replica sync counter>[5m]) == 0 and increase(<database/WAL change counter or txid>[5m]) > 0`;
  - if no such counter pair exists, use `increase(<sync error counter>[5m]) > 0`, name it `LitestreamErrors`, and add a comment saying why.

Validate the rules and the config:
```bash
docker compose -p central -f deploy/compose/central.yml exec prometheus promtool check rules /etc/prometheus/alerts.yml
docker compose -p central -f deploy/compose/central.yml exec prometheus promtool check config /etc/prometheus/prometheus.yml
docker compose -p central -f deploy/compose/central.yml restart prometheus
```
Expected: both report `SUCCESS`, and `http://127.0.0.1:9090/alerts` lists six rules, none firing.

- [ ] **Step 6: Stop and commit**

```bash
make down
git add -A && git commit -qm "build: compose files for a node and the local cluster, Garage and Prometheus"
```

---

### Task 12: End-to-end test and restore drill

Spec §7.3 and §12 (the e2e row).

**Files:**
- Create: `scripts/restore-drill.sh`, `scripts/e2e.sh`
- Modify: `Makefile` (`restore-drill`, `e2e`)

**Interfaces:**
- Consumes: `scripts/cluster.sh` (Task 11), the images (Task 10), `library-web check-db` (Task 4) and the worker's metrics (Task 9).

- [ ] **Step 1: The restore drill**

`scripts/restore-drill.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# Restore drill (spec §7.3): restore a node's newest backup into a scratch
# volume, check it, and show its row counts next to the live node's. The
# running node is never touched. Run it regularly in production (RUNBOOK: weekly).
#   scripts/restore-drill.sh <node> [env-file]
set -euo pipefail
cd "$(dirname "$0")/.."
node=${1:?usage: restore-drill.sh <node> [env-file]}
env_file=${2:-deploy/compose/local/$node.env}
PREFIX="${PREFIX:-}"
NETWORK="${SHARED_NETWORK:-${PREFIX}sqlite-prod}"
LITESTREAM_IMAGE="${LITESTREAM_IMAGE:-sqlite-production/litestream:dev}"
WEB_IMAGE="${WEB_IMAGE:-sqlite-production/web:dev}"
volume="${PREFIX}drill-${node}-$$"

# The scratch volume is mounted at /data, whose owner in the images (uid
# 10001) is copied into a new volume, so the restore can write there.
trap 'docker volume rm -f "$volume" >/dev/null' EXIT
docker volume create "$volume" >/dev/null
net=()
if docker network inspect "$NETWORK" >/dev/null 2>&1; then net=(--network "$NETWORK"); fi

echo "== restoring the newest backup of $node into a scratch volume"
docker run --rm --env-file "$env_file" -e NODE_ID="$node" "${net[@]}" -v "$volume:/data" \
  --entrypoint litestream "$LITESTREAM_IMAGE" \
  restore -config /etc/litestream.yml -integrity-check full /data/library.db

echo "== checking the restored copy"
if ! docker run --rm -v "$volume:/data" "$WEB_IMAGE" check-db; then
  echo "DRILL FAILED: the restored copy did not pass check-db" >&2
  exit 1
fi

echo "== the live node, for comparison (it keeps changing, so small differences are normal)"
docker compose -p "${PREFIX}${node}" -f deploy/compose/node.yml --env-file "$env_file" \
  exec -T web library-web check-db || echo "(live node not reachable from here)"
echo "DRILL PASSED"
```

- [ ] **Step 2: The end-to-end test**

`scripts/e2e.sh` (`chmod +x`):
```bash
#!/usr/bin/env bash
# End-to-end test of the whole system (spec §12). It starts a separate,
# throwaway cluster (projects e2e-central and e2e-node1..3, web on ports
# 4101-4103), runs five scenarios, and deletes the cluster again. Your own
# `make up` cluster is not touched. Needs Docker, curl, jq and `make images`.
set -euo pipefail
cd "$(dirname "$0")/.."

export PREFIX=e2e-
export SHARED_NETWORK=e2e-sqlite-prod
export POSTGRES_HOST_PORT=5434
export PROMETHEUS_HOST_PORT=9091
export HTTP_PORT_BASE=4100
NODE_FILES=(-f deploy/compose/node.yml -f deploy/compose/local/node.override.yml)
WORK=$(mktemp -d)

cleanup() {
  NODE_ID=node9 docker compose -p e2e-node9 "${NODE_FILES[@]}" --env-file deploy/compose/local/node2.env down -v >/dev/null 2>&1 || true
  scripts/cluster.sh reset --yes >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

step() { printf '\n==> %s\n' "$*"; }
fail() { echo "E2E FAILED: $*" >&2; exit 1; }

# wait_until <seconds> <what> <command...>: retry the command every second.
wait_until() {
  local deadline=$((SECONDS + $1)) what=$2
  shift 2
  until "$@" >/dev/null 2>&1; do
    ((SECONDS < deadline)) || fail "timed out waiting for: $what"
    sleep 1
  done
}

port() { echo $((HTTP_PORT_BASE + ${1#node})); }
node_compose() {
  local node=$1
  shift
  HTTP_PORT=$(port "$node") docker compose -p "e2e-$node" "${NODE_FILES[@]}" --env-file "deploy/compose/local/$node.env" "$@"
}
node9_compose() { NODE_ID=node9 HTTP_PORT=4109 docker compose -p e2e-node9 "${NODE_FILES[@]}" --env-file deploy/compose/local/node2.env "$@"; }
central() { docker compose -p e2e-central -f deploy/compose/central.yml "$@"; }
sql() { central exec -T postgres psql -U sync -d library -tAc "$1"; }
healthy() { curl -fsS "http://127.0.0.1:$(port "$1")/api/health"; }
worker_healthy() { node_compose "$1" exec -T sync-worker curl -fsS http://127.0.0.1:9100/health; }
metric() { node_compose "$1" exec -T sync-worker curl -fsS http://127.0.0.1:9100/metrics | awk -v name="$2" '$1 == name { print $2 }'; }
in_postgres() { [ "$(sql "SELECT count(*) FROM books WHERE node_id = '$1' AND title = '$2'")" = 1 ]; }
count_is() { [ "$(sql "SELECT count(*) FROM $2 WHERE node_id = '$1'")" = "$3" ]; }
targets_up() {
  curl -fsS "http://127.0.0.1:$PROMETHEUS_HOST_PORT/api/v1/targets" |
    jq -e '[.data.activeTargets[] | select(.health == "up")] | length == 6'
}

login() {
  node_compose "$1" exec -T web sh -c 'printf "password123\n" | library-web create-user admin --password-stdin' >/dev/null
  curl -fsS -c "$WORK/$1.cookies" -H 'content-type: application/json' \
    -d '{"username":"admin","password":"password123"}' "http://127.0.0.1:$(port "$1")/api/auth/login" >/dev/null
}
add_book() {
  jq -n --arg title "$2" '{title: $title, author: "E2E", isbn: null, published_year: 2026, total_copies: 1}' |
    curl -fsS -b "$WORK/$1.cookies" -H 'content-type: application/json' -d @- \
      "http://127.0.0.1:$(port "$1")/api/books" >/dev/null
}

step "start the cluster"
scripts/cluster.sh up
for n in node1 node2 node3; do wait_until 120 "$n healthy" healthy "$n"; done
for n in node1 node2 node3; do wait_until 120 "$n's worker healthy" worker_healthy "$n"; done

step "1. writes on every node reach Postgres under the right node_id"
node_compose node1 exec -T web library-web seed >/dev/null
wait_until 30 "node1's 12 seeded books in Postgres" count_is node1 books 12
for n in node1 node2 node3; do
  login "$n"
  add_book "$n" "E2E $n"
done
for n in node1 node2 node3; do wait_until 30 "the book from $n in Postgres" in_postgres "$n" "E2E $n"; done
count_is node2 books 1 || fail "node2 should have exactly its own book"
wait_until 60 "Prometheus scraping all 6 targets" targets_up

step "2. VM lost: delete node2's disk and start it again"
node_compose node2 down -v # a clean stop: Litestream uploads the last changes first
node_compose node2 up -d   # ALLOW_FRESH_DB=false: it must restore from S3
wait_until 120 "node2 healthy after the restore" healthy node2
node_compose node2 logs restore | grep "restored /data/library.db" >/dev/null || fail "node2 was not restored from S3"
curl -fsS -b "$WORK/node2.cookies" "http://127.0.0.1:$(port node2)/api/books?q=E2E" | jq -e '.total == 1' >/dev/null ||
  fail "node2's book (and its login session) should be back"
wait_until 120 "node2's worker healthy" worker_healthy node2
[ "$(metric node2 sync_resyncs_total)" = 0 ] || fail "a clean restore must not need a resync"
add_book node2 "E2E node2 after restore"
wait_until 30 "node2's new book in Postgres" in_postgres node2 "E2E node2 after restore"

step "3. an empty disk with no backup refuses to start"
ALLOW_FRESH_DB=false node9_compose up -d >/dev/null 2>&1 || true
node9_compose logs restore 2>&1 | grep "No database and no backup for node 'node9'" >/dev/null || fail "the restore gate should refuse node9"
if node9_compose ps --status running --services 2>/dev/null | grep -x web >/dev/null; then
  fail "web must not start when the restore gate refuses"
fi

step "4. Postgres down: the app keeps working and the worker catches up"
central stop postgres
add_book node1 "E2E during outage" # succeeds: the app never needs Postgres
sleep 5
central start postgres
wait_until 120 "the outage book in Postgres" in_postgres node1 "E2E during outage"

step "5. restore drill"
scripts/restore-drill.sh node1

printf '\nE2E PASSED\n'
```
Add to `Makefile`, with both names in `.PHONY`:
```make
restore-drill: ## Restore a node's backup to a scratch copy and check it: make restore-drill NODE=node2
	scripts/restore-drill.sh $(or $(NODE),node1)

e2e: images ## End-to-end test on a separate, throwaway cluster
	scripts/test-restore-gate.sh
	scripts/e2e.sh
```

- [ ] **Step 3: Run it**

Run: `make e2e`
Expected:
- `restore gate: 12 checks passed`;
- then five `==>` steps;
- `DRILL PASSED`;
- `E2E PASSED`.

After it finishes, `docker ps -a --filter name=e2e-` lists nothing.

If a scenario fails, fix the cause (code, compose or script logic) and run the e2e test again. Never loosen what a scenario checks.

- [ ] **Step 4: Run the drill against the normal cluster, then commit**

```bash
make up && make seed NODE=node2 && sleep 3 && make restore-drill NODE=node2 && make down
git add -A && git commit -qm "test: end-to-end test of the cluster and the restore drill"
```
Expected: `DRILL PASSED`, with `books: 12` for both the copy and the live node.

---

### Task 13: Documents, CI and README

Spec §13 (documents), §12 (CI).

**Files:**
- Create:
  - `README.md`
  - `docs/ARCHITECTURE.md`
  - `docs/RUNBOOK.md`
  - `docs/decisions/0001-sqlite-as-the-primary-database.md`
  - `docs/decisions/0002-one-container-per-job.md`
  - `docs/decisions/0003-expand-and-contract-migrations.md`
  - `docs/decisions/0004-change-log-written-by-triggers.md`
  - `docs/decisions/0005-tokens-and-epochs-detect-restores.md`
  - `docs/decisions/0006-garage-instead-of-minio.md`
  - `.github/workflows/ci.yml`
- Modify: `Makefile` (final `check`)

**Writing rules for every document:**
- Plain words and short sentences. Use a table where it is a list of facts.
- Each fact must match the code. When you say where something happens, link to the line (`[changes.rs](../apps/sync-worker/src/changes.rs#L42)`); Step 6 checks every link.

- [ ] **Step 1: `README.md`**

Sections, in order:
1. **What this is.** Two or three sentences: SQLite as each node's production database, Litestream to S3, a sync worker to Postgres. Link to `docs/ARCHITECTURE.md` and the spec.
2. **You need.** Rust stable, Node 22, Docker 25+ with Compose v2, `sqlx-cli` (install line from `make setup`), curl, jq.
3. **Coding locally (no containers).** `make setup`, `make dev`, `make dev-frontend`, `make seed-dev`, `make user-dev NAME=alice`. Open http://localhost:5173.
4. **The three-node cluster.**
   - `make up`, then `make user NODE=node1`, then open http://localhost:4001 (and 4002, 4003).
   - `make seed NODE=node1`.
   - `make psql` with the query `SELECT node_id, count(*) FROM books GROUP BY 1;`.
   - Prometheus at http://127.0.0.1:9090.
   - `make down` and `make reset`.
5. **Tests.** What each runs:
   - `make test`: Rust tests, which start a throwaway Postgres in Docker;
   - `make check`: every CI gate except e2e;
   - `make e2e`: restore-gate test plus the full cluster scenarios.
6. **Folders.** A table with one row per folder from spec §4 and its one-line job.
7. **Ports.** The table from Global Constraints.
8. **Read next.** ARCHITECTURE, RUNBOOK, decisions.

- [ ] **Step 2: `docs/ARCHITECTURE.md`**

Sections, in order:
1. **The picture.** The diagram from spec §2, and what each of the two backups is for.
2. **One write, step by step.** Link each step to its code:
   - `POST /api/books` (`apps/web/src/books/routes.rs`);
   - the write: a single `INSERT` for a new book; updates and deletes run in a `BEGIN IMMEDIATE` transaction (`apps/web/src/books/sqlite.rs`);
   - the trigger adds a change_log row (`migrations/sqlite/20261011000000_change_log.sql`);
   - commit;
   - Litestream uploads the WAL within about 1 s;
   - the worker reads the batch within at most 2 s (`changes.rs` `read_batch`);
   - one Postgres transaction applies the batch and moves the checkpoint (`mirror.rs` `apply_batch`);
   - the prune (`changes.rs` `prune`).
3. **Guarantees.** A table:

   | Situation | What can be lost |
   |---|---|
   | Lost disk | about 1 s of writes |
   | Postgres | never loses what the node still has; follows the node after a restore and keeps the rest in `sync_archive` |
   | App during an outage | keeps working when Postgres or S3 is down |

   Also say what Postgres is (a readable copy for reports and last-resort recovery) and is not (a source for restoring a node; it has no user accounts).
4. **SQLite rules.** Rules S1–S16 from spec §5, each with a link to where it is enforced.
5. **How a restore is detected.** Tokens and epochs: the spec §6.4 table in plain words, plus what a resync does (§6.5).
6. **Module map.** One line per module of `apps/sync-worker/src` (`config`, `tables`, `changes`, `mirror`, `sync`, `schema`, `run`, `metrics`, `http`, `status`, `cli`, `telemetry`) and per folder of `apps/web/src`.
7. **Adding a synced table.** The three steps from spec §6.9 and the contract test that checks them.
8. **Future work.**
   - chunked full copy for large databases;
   - a history table;
   - zero-downtime deploys;
   - separate Postgres roles for migrations and runtime;
   - host metrics with disk alerts;
   - Alertmanager.

- [ ] **Step 3: `docs/RUNBOOK.md`**

One section per procedure. Each section has three parts:
- **When**: the symptom or alert.
- **Steps**: numbered, with exact commands for a VM running `docker compose -p <node> -f node.yml --env-file node.env`.
- **Check it worked.**

Use these headings exactly. The alert links in `alerts.yml` point at their anchors.
1. `## Add a node`. First start with `ALLOW_FRESH_DB=true`, then set it back to `false`.
2. `## Deploy a new version`. One node at a time. Watch `/api/health`, then `sync-worker status`. Migration rules: expand, then contract.
3. `## Replace a lost VM`.
4. `## Restore to a point in time`.
   - When: bad migration or bad data.
   - Steps: stop the node, delete `library.db` with `-wal`/`-shm`, then `litestream restore -timestamp …` through the restore image, then start.
   - Expect a resync, and read the next section.
5. `## After a resync`. How to list rows that only existed in the lost history:

   ```sql
   SELECT a.table_name, a.row_data FROM sync_archive a
   WHERE a.node_id = 'node2' AND a.epoch = 1 AND NOT EXISTS (
     SELECT 1 FROM books b
     WHERE a.table_name = 'books' AND b.node_id = a.node_id
       AND b.id = (a.row_data->>'id')::bigint AND to_jsonb(b) - 'synced_at' = a.row_data - 'synced_at');
   ```

   Then decide with the branch whether to re-enter them.
6. `## Postgres down`.
7. `## S3 down`.
8. `## Duplicate NODE_ID`.
   - Symptom: worker log line "another sync-worker holds the lock".
   - The Litestream risk to the backup folder.
   - Fix: stop one node and give it a unique id.
9. `## Disk filling up`.
   - Check WAL size and change log size (`sync-worker status`).
   - Common cause: a long read transaction, or Postgres down for days.
10. `## Database file missing but its WAL is still there`. The restore gate's refusal from spec §7.2 step 3.
11. `## Run the restore drill`. Weekly.
12. `## Check a database for corruption`. `library-web check-db`.
13. `## Production to-dos`.
    - TLS in front of the web app;
    - a secrets manager;
    - separate Postgres roles;
    - host and disk metrics;
    - Alertmanager;
    - S3 bucket versioning and lifecycle rules;
    - a fixed branch-to-node routing rule.

- [ ] **Step 4: The six decision records**

Each file has `# <number>. <title>`, then `Status: accepted (2026-10-11)`, then three sections: **Context**, **Decision**, **Consequences**. Each is under one page. Take the content from the spec:

| Record | Source in the spec |
|---|---|
| 0001 | §1 and §2. Why SQLite primary per node; sticky users; alternatives named in the spec (single Postgres with replicas, Litestream alone, PowerSync/ElectricSQL) and why they were not chosen. |
| 0002 | §3, "Why four containers and not one". |
| 0003 | §5 S10. |
| 0004 | §6.1 and §6.3. Triggers instead of app code; shipping current row values instead of payloads; coalescing. |
| 0005 | §6.4 and §6.5. |
| 0006 | §10. MinIO's community edition is archived; Garage is S3-compatible and written in Rust. |

- [ ] **Step 5: CI and the final `check` target**

`.github/workflows/ci.yml`:
```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:

env:
  CARGO_TERM_COLOR: always
  # apps/web's compile-time checked SQL reads the committed .sqlx/ data.
  SQLX_OFFLINE: "true"

jobs:
  rust:
    name: Rust (fmt, clippy, tests with Postgres)
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:18.6
        env:
          POSTGRES_PASSWORD: test
        ports: ["55432:5432"]
        options: >-
          --health-cmd "pg_isready -h 127.0.0.1 -U postgres"
          --health-interval 2s --health-timeout 5s --health-retries 30
    env:
      TEST_POSTGRES_URL: postgres://postgres:test@127.0.0.1:55432/postgres
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --check
      - run: cargo clippy --workspace --all-targets -- -D warnings
      - run: cargo test --workspace
      - run: cargo build -p library-web -p sync-worker
      - name: The web app has no Postgres driver
        run: test -z "$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)"

  types:
    name: Generated TypeScript types are up to date
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test -p library-shared --features ts
      - name: Fail if regenerating changed anything
        run: git diff --exit-code -- apps/frontend/src/generated

  frontend:
    name: React (lint, typecheck, test, build)
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: apps/frontend
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 22
          cache: npm
          cache-dependency-path: apps/frontend/package-lock.json
      - run: npm ci
      - run: npm run lint
      - run: npm run typecheck
      - run: npm test
      - run: npm run build

  e2e:
    name: Images, restore gate and end-to-end
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - run: make e2e
```
Final `check` target in `Makefile`. It replaces the existing one and keeps the no-Postgres line from Task 10:
```make
check: ## Every CI gate except the Docker end-to-end test (make e2e)
	cargo fmt --check
	cargo clippy --workspace --all-targets -- -D warnings
	$(MAKE) test
	SQLX_OFFLINE=true cargo build -p library-web -p sync-worker
	@test -z "$$(cargo tree -q -p library-web -i sqlx-postgres 2>/dev/null)" || { echo "library-web must not depend on a Postgres driver"; exit 1; }
	cargo test -q -p library-shared --features ts && git diff --exit-code -- apps/frontend/src/generated
	cd apps/frontend && npm run lint && npm run typecheck && npm test
```

- [ ] **Step 6: Check every link and line reference in the documents**

```bash
cd ~/rust/rust-sqlite-demo
# Every relative link points at a file that exists, and every #L<n> is within the file.
grep -oE '\]\(\.{0,2}/?[^)#: ]+(#L[0-9]+)?\)' README.md docs/*.md docs/decisions/*.md | while IFS= read -r hit; do
  doc=${hit%%:*}; link=${hit#*](}; link=${link%)}
  path=${link%%#*}; line=${link#*#L}; [ "$line" = "$link" ] && line=
  target=$(cd "$(dirname "$doc")" && realpath -m "$path")
  [ -e "$target" ] || { echo "BROKEN $doc -> $link"; continue; }
  if [ -n "$line" ] && [ "$(wc -l < "$target")" -lt "$line" ]; then echo "PAST END $doc -> $link"; fi
done
```
Expected: no output. Then open five of the `#L` links at random and confirm each line shows what the sentence says.

- [ ] **Step 7: Run every gate and commit**

```bash
make check
make e2e
git add -A && git commit -qm "docs: README, architecture, runbook, decision records, and CI"
```
Expected: `make check` passes and `make e2e` ends with `E2E PASSED`.

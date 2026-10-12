# Rails comparison app: design

Date: 2026-10-13. Status: approved in conversation (approach A + Puma cluster).

## Goal

Measure how many req/s a Ruby on Rails version of the library API sustains on the same load-test box as the Rust (axum) version, with CPU and memory per process, so the two can be compared directly.

**Same work, idiomatic Rails.** The Rails app does exactly what the Rust app does: the same HTTP API, the same SQL shape, the same total-count caching rules, the same locking, the same Argon2id hashing limits. It is written the normal Rails way (ActiveRecord models, controllers, plain JSON rendering); it drops to SQL (`find_by_sql`, `select_value`, Arel) only where ActiveRecord cannot express the same query. Differences in req/s then come from Ruby and Rails, not from one app doing less work.

## What must match the Rust app

The source of truth is the Rust code (`apps/web/src`) and the API section of `docs/superpowers/specs/2026-10-10-sqlite-production-design.md` (§5). `docs/API-CONTRACT.md` (written as part of this work) states the contract in one place.

- Every route under `/api` (auth login/logout/me; books, members: list/create/show/update/delete; loans: list/create, return) and the health route, with the same methods, paths, status codes, JSON field names and types, and the §5.5 error shape (including field errors and the JSON 404 for unknown `/api/*` paths).
- Sessions: the same cookie (name, attributes), random token stored hashed in `sessions`, same TTL and expiry check, expired-session sweep on login, logout deletes the session.
- Passwords: Argon2id, verify the PHC strings already in the database (`m=19456,t=2,p=1`); a dummy verification for unknown users. **At most one hash at a time across the whole app** (all Puma workers): a cross-process lock (e.g. `flock` on a file), matching the Rust app's one permit on 2 cores.
- Lists: cursor paging and offset paging with the same parameters, page sizes and `next_cursor` format; search (`q`, the same Postgres full-text and prefix behaviour, including the MATERIALIZED CTE and `COUNT(*) OVER ()` totals); availability filter and its totals from active loans.
- Totals: the same queries, cached with the same TTL. The cache is **per Puma worker** (in-process memory store). With 2 workers the counts refresh about twice as often as in Rust; that is a real cost of the Rails process model and is reported, not hidden.
- Writes: the same transactions and row locks (`SELECT … FOR UPDATE` where Rust takes them), the same 409 business answers (no copies left, book still on loan, …).

## Architecture

- `apps/rails/`: Rails 8 API-only app, Ruby 4.0 (official `ruby:4.0.5-slim` image), YJIT on.
- **Puma cluster: `WEB_CONCURRENCY=2` workers × `RAILS_MAX_THREADS=5` threads**, preload_app. ActiveRecord pool 5 per worker, so 10 Postgres connections in all, like the Rust pool.
- **Schema:** owned by the existing SQL migrations (`migrations/web-postgres`). Rails runs no migrations and does not dump `schema.rb`; models map onto the existing tables.
- Listens on port 4000 with `DATABASE_URL`, like the Rust web container, so nginx and k6 are unchanged.
- Production settings: `RAILS_ENV=production`, logging at info without per-request lines in the load test (the Rust app logs no per-request lines either), no asset pipeline, no Action Cable, no Active Storage, no Solid Queue/Cache/Cable.
- Image: `apps/rails/Dockerfile`, multi-stage, gems installed without dev/test groups.

## Load-test integration

- `LT_APP=rust` (default) or `LT_APP=rails` in `scripts/loadtest.sh` selects the web image; everything else (nginx, Postgres, k6 script, protocol, seed snapshot, cores, clock lock) is unchanged.
- Box memory stays 4096 MiB; the Rails default split is chosen from measured idle + loaded RSS (expected web ~1 GB for 2 workers) and shown in the summary header.
- The summary header and results record which app ran.

## Correctness

- **Contract test** (`scripts/contract-test.sh`): runs the same scripted HTTP requests against a running box and checks status codes and JSON shapes/values per `docs/API-CONTRACT.md`. It must pass against the Rust app first (it describes Rust), then against Rails. Run before any Rails load round.
- Rails' own request tests (Minitest) for each endpoint, run in Docker against a Postgres test database built from the same SQL migrations.

## Out of scope

The SQLite variant, the sync worker, the frontend, changing the Rust app.

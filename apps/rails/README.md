# library-rails: the Rails copy of the API

A Rails 8.1 API-only app on Ruby 4.0 (YJIT on) that answers the same HTTP API as
`apps/web`, with the same SQL, on Postgres only. It exists for the load-test
comparison (`docs/superpowers/specs/2026-10-13-rails-comparison-design.md`): same
work, written the normal Rails way, so a difference in req/s comes from Ruby and
Rails. The contract it follows is `docs/API-CONTRACT.md`; `scripts/contract-test.sh`
checks a running copy against it.

The schema belongs to `migrations/web-postgres` (plain SQL). Rails runs no
migrations and dumps no `schema.rb` (`dump_schema_after_migration = false`); the
models map onto the existing tables.

## Build

```bash
cd apps/rails
docker build -t library-rails:lt .
```

Multi-stage: gems are built in a stage with compilers (the `argon2` gem is native),
the final image has the gems, the code, `curl` and a non-root user (uid 10001). It
has no dev or test gems (the Gemfile has none: Minitest and Rack::Test come with
Rails). Do not build while a load round is in load.

## Run (the container contract)

It replaces the Rust web container in `deploy/compose/local/loadtest.web.yml`:

| | |
|---|---|
| Listens on | `0.0.0.0:4000` (`PORT`) |
| `DATABASE_URL` | `postgres://user:pass@host:5432/db`, an already migrated database |
| `WEB_CONCURRENCY` | Puma workers, default 2 (preload, copy-on-write) |
| `RAILS_MAX_THREADS` | threads per worker, default 5; the ActiveRecord pool is the same number per worker |
| `COOKIE_SECURE`, `SESSION_TTL_HOURS` | as in the Rust app (default `false`, 168) |
| `LIST_COUNT_TTL_MS`, `NODE_ID` | as in the Rust app (1000, `local`) |
| `SECRET_KEY_BASE` | has a local-only default; nothing signed or encrypted depends on it |
| `RAILS_LOG_LEVEL` | default `warn`: no per-request lines (`info` brings them back) |
| `RAILS_ENV` | `production` in the image |
| Health | `GET /api/health`, `curl` is installed for the compose health check |

## Test

The request tests (Minitest, `test/`) run in Docker against a Postgres container
loaded with the SQL migrations. Every test runs in a transaction that is rolled back.

```bash
docker network create lt-rb-net
docker run -d --name lt-rb-test-pg --network lt-rb-net --cpuset-cpus 6-9 \
  -e POSTGRES_USER=library -e POSTGRES_PASSWORD=library -e POSTGRES_DB=library_test \
  postgres:18.6 -c fsync=off
# (wait a few seconds)
for f in ../../migrations/web-postgres/*.sql; do
  docker exec -i lt-rb-test-pg psql -q -v ON_ERROR_STOP=1 -U library -d library_test < $f
done

docker build --target test -t library-rails:test .
docker run --rm --network lt-rb-net --cpuset-cpus 6-9 \
  -e DATABASE_URL=postgres://library:library@lt-rb-test-pg:5432/library_test \
  library-rails:test
```

`TEST_LOG=1` prints the app log (internal errors) while testing. The tests do not
hash with Argon2 to log in, except `auth_test.rb`, which verifies a hash made by the
Rust `create-user` command (`m=19456,t=2,p=1`).

Against a running copy, `scripts/contract-test.sh http://HOST:4000` is the
acceptance test (see "Contract test" in `docs/LOADTEST.md`).

## What maps to which Rust module

| Rust (`apps/web/src`) | Rails (`apps/rails`) |
|---|---|
| `app.rs` (router, 404/405 JSON, body limit, headers, compression) | `config/routes.rb`, `lib/body_limit.rb`, `lib/trailing_slash.rb`, `config/application.rb` (headers, `Rack::Deflater`), `ErrorsController` |
| `error.rs` (`AppError`, the error shape) | `app/errors/api_error.rb`, `ApplicationController` (`rescue_from`) |
| `extract.rs` (typed body, query, path) | `ApplicationController` (`json_body`, `body_*`, `query_*`, `path_id`) |
| `health.rs` | `HealthController` |
| `auth/` (routes, extractor, service, `postgres.rs`) | `AuthController`, `ApplicationController#require_user!`, `Authentication`, `User`, `UserSession` |
| `auth/password.rs` (Argon2id, permits) | `PasswordHasher` (`argon2` gem; one hash at a time across all workers: `flock` on `/tmp/library-password-hash.lock`) |
| `books/` (`domain.rs`, service, `postgres.rs`) | `BooksController`, `Book` (validations, `list`, `update_checked`, `delete_checked`), `Isbn` |
| `members/` | `MembersController`, `Member`, `Email` |
| `loans/` | `LoansController`, `Loan` (`list`, `borrow`, `mark_returned`) |
| `pagination.rs` | `Pagination`, `Text` |
| `count_cache.rs` | `CountCache` (per worker, in memory) |
| `db.rs` (pool, constraint names) | `config/database.yml` (pool = threads), `ApplicationController#render_conflict`, `SqlBuilder` |
| `clock.rs`, `config.rs` | `Clock`, `Settings` |

## Where it is deliberately not the same

- **ActiveRecord does the CRUD** (`create`, `update!`, `destroy!`, `lock`, `where`,
  `count`, `pick`). The list and one-row queries are raw SQL through `SqlBuilder`
  (numbered `$n` binds, prepared statements), because ActiveRecord cannot express
  the deferred join, the `MATERIALIZED` CTE with `COUNT(*) OVER ()` or the row
  comparison cursors. `AR` wraps a `save` in `BEGIN`/`COMMIT`; the Rust INSERT runs
  alone.
- **The count cache is per Puma worker**, so with 2 workers the totals refresh about
  twice as often as in Rust.
- Rails' default API middleware stays (request id, `Rack::ETag`, `ConditionalGet`,
  the executor, the query cache); `Rack::Deflater` gzips every response when asked.
- No request timeout (Rust answers 408 after 15 s), and the 64 KiB limit looks at
  `Content-Length` only (a chunked body is not limited).
- The error `message` of a 400 is Rails' or ours, not axum's (the contract leaves it
  open, except `invalid cursor`).
- `Content-Type` is `application/json` (no charset), as in Rust.

# library-jobs

A Postgres job queue with a transactional outbox. A job is a row you insert **with your own
connection, inside your own transaction**, so it exists exactly when your data change commits.
Worker processes claim jobs with `FOR UPDATE SKIP LOCKED` and hold each one by a lease that a
single heartbeat per process renews. A worker that dies loses its jobs to the others. A worker that
lost a job cannot write its result.

The crate has no web framework in its core. It needs a tokio runtime and a sqlx 0.9 Postgres pool.
Its only framework code is an optional axum router for `/metrics`, `/healthz` and `/readyz`.

## Contents

- [Requirements](#requirements)
- [The three pieces](#the-three-pieces)
- [Quick start](#quick-start)
- [tokio](#tokio)
- [axum](#axum)
- [Other web frameworks](#other-web-frameworks)
- [Testing your jobs](#testing-your-jobs)
- [Features](#features)
- [Guarantees and limits](#guarantees-and-limits)

## Requirements

| | |
|---|---|
| Database | Postgres. Tested on 18.6. Uses `SKIP LOCKED`, `jsonb`, partial unique indexes and advisory locks. |
| Driver | sqlx 0.9 with the `postgres` feature. Runtime queries only; no `DATABASE_URL` at build time. |
| Runtime | tokio 1. A multi-threaded runtime is recommended (see [tokio](#tokio)). async-std and smol are not supported. |
| Rust | Edition 2024. |

## The three pieces

| Piece | Where it runs | What it needs |
|---|---|---|
| `Enqueuer` | In your request handlers, inside your transactions | A `&mut PgConnection` (a transaction derefs to one) |
| `Worker` | In its own process (`your-app worker`), or spawned inside your server | A `PgPool` of its own, a `Registry` of job kinds, a `WorkerConfig` |
| `library_jobs::ops` | A CLI or an admin page | A `PgPool` |

Only the `Worker` runs job code. A web server that only enqueues needs `Enqueuer` and
nothing else: no background tasks, no extra connections.

## Quick start

A job is a serializable struct with a kind (versioned: change the version when the
arguments change shape) and a queue:

```rust
use library_jobs::{Job, JobContext, JobError, JobOptions};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Serialize, Deserialize)]
pub struct WelcomeEmail {
    pub user_id: i64,
}

impl Job for WelcomeEmail {
    const KIND: &'static str = "welcome_email.v1";
    const QUEUE: &'static str = "email";

    fn options() -> JobOptions {
        JobOptions { deadline: Duration::from_secs(120), ..JobOptions::default() }
    }

    // While one is waiting or running, a second enqueue returns `Duplicate`.
    fn unique_key(&self) -> Option<String> {
        Some(self.user_id.to_string())
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        let mailer = ctx.require::<Mailer>()?; // provided at registration
        let email: String = sqlx::query_scalar("SELECT email FROM users WHERE id = $1")
            .bind(self.user_id)
            .fetch_one(ctx.pool())
            .await?;
        mailer.send_welcome(&email, ctx.id()).await.map_err(JobError::retry)
    }
}
```

`JobError::Retry` retries with backoff (5 s doubling to 1 h, full jitter, both set by
`JobOptions::backoff`), `RetryAfter(d, msg)` not before `d`, `Permanent` never, and `Cancelled` ends the
job `cancelled`. No retry delay is longer than 7 days (`MAX_RETRY_DELAY`), whatever the backoff
or `RetryAfter` says. A panic counts as a retryable error. After `max_attempts` (10) the job ends
`failed`.

Migrate once at start-up, after your own migrations. The crate keeps its own migration table
(`_jobs_migrations`), so it never touches yours:

```rust
sqlx::migrate!().run(&pool).await?; // your schema
library_jobs::migrate(&pool).await?; // the jobs schema
```

Enqueue inside the transaction that makes the change:

```rust
let jobs = library_jobs::Enqueuer::new(true); // true: NOTIFY workers on commit
let mut tx = pool.begin().await?;
let id: i64 = sqlx::query_scalar("INSERT INTO users (email) VALUES ($1) RETURNING id")
    .bind(email)
    .fetch_one(&mut *tx)
    .await?;
jobs.enqueue(&mut tx, &WelcomeEmail { user_id: id }).await?;
tx.commit().await?; // the job and the user commit together; a rollback drops both
```

Run a worker:

```rust
use library_jobs::{QueueConfig, Registry, Worker, WorkerConfig};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

let registry = Registry::new()
    .register::<WelcomeEmail>()
    .provide(Arc::new(Mailer::from_env()?));
let config = WorkerConfig::new(QueueConfig::parse_list("email:4")?); // 4 handlers at once
let worker = Worker::new(worker_pool, registry, config)?;
let stop = CancellationToken::new();
let reason = worker.run(stop.clone()).await?; // until `stop` is cancelled
```

`run` returns `ExitReason::Shutdown` after a graceful stop, or `ExitReason::SelfFenced` when
the process's registry row was deleted: the maintenance leader pruned it after it was silent
for `prune_after` (3 minutes by default), as happens to a suspended VM or a frozen container.
On `SelfFenced`, exit non-zero and let your supervisor restart the process; its handlers were
cancelled and any result they still write is rejected, because the job is no longer theirs.

A database outage is not a reason to exit. See
[Guarantees and limits](#guarantees-and-limits) for what the worker does meanwhile.

## tokio

**Runtime flavour.** The worker spawns one task per queue, one per running job, a heartbeat
task, a LISTEN task and a leader candidate. A `current_thread` runtime runs them all on one
thread, so a handler that blocks or burns CPU stalls the heartbeat. Once no beat has succeeded
for the shortest lease minus one heartbeat (40 s with the defaults), the worker cancels every
handler and stops claiming until a beat succeeds, because the lease could expire next. Use the
multi-threaded runtime (`#[tokio::main]`'s default).

**Handlers are `Send + 'static`.** `perform` takes `self` and returns a `Send` future. Hold no
`std::sync::MutexGuard` across an `.await`; share state through `ctx.require::<T>()`, where
`T: Send + Sync`.

**Blocking and CPU-heavy work.** Wrap blocking calls in `tokio::task::spawn_blocking`. For
CPU-heavy jobs (image resizing, report rendering), give them a queue of their own with few
slots, e.g. `reports:1`, so they cannot take every core, and run the work in `spawn_blocking`
or on a rayon pool. Slots are per process; `name:slots:max_running` also caps a queue across
all processes.

**Cancellation.** `ctx.cancelled()` is a future that resolves when an operator cancels the
job, the process shuts down, or the lease is lost. Long handlers `select!` on it:

```rust
tokio::select! {
    result = do_the_work(&ctx) => result,
    () = ctx.cancelled() => Err(JobError::Cancelled),
}
```

A handler that ignores it is stopped by its deadline (`JobOptions::deadline`, 15 min by
default): the worker aborts its task, which drops the future at its next `.await`, and records
`deadline_exceeded`. A handler that never yields (a blocking call outside `spawn_blocking`)
cannot be aborted and keeps its thread; the heartbeat stops renewing the job's lease at the
deadline, so the job is reaped and retried elsewhere.

**Time.** Every stored time is the database's `now()`. Workers on several machines never
compare their clocks. `tokio::time` is used only for local waits (poll interval, heartbeat
period).

**Graceful shutdown.** Cancel the `CancellationToken` passed to `run`. The worker stops
claiming, waits up to `shutdown_timeout` (25 s) for running jobs, cancels the rest, and waits
`cancel_grace` (5 s). A handler that stops after that cancellation is released back to the
queue without spending an attempt; one that ignores it keeps its lease until it expires, and
the reaper retries the job. Then the worker deletes its registry row. Give the process at
least `shutdown_timeout + cancel_grace + 10 s` before a SIGKILL (Docker's
`stop_grace_period: 40s`). The worst case is longer: the worker first waits up to 5 s for its
helper tasks (queue loops, LISTEN, leader), which end at once unless one is stuck on a dead
connection, then up to `shutdown_timeout` for the handlers, then `cancel_grace`, plus up to 5 s
each for the registry writes; leave `shutdown_timeout + cancel_grace + 20 s` if a stuck database
must not cost you a SIGKILL.
An HTTP drain started by the same token runs alongside this, so it only adds time when its
budget is longer. A killed process loses no job: they are reaped when their leases expire (each
such expiry counts as a crash, see below).

**A runtime of its own.** When the worker runs inside your web server, CPU-heavy jobs can
still slow requests: they share the runtime's threads. Run the worker on a separate runtime
in its own OS thread. Create its pool **inside** that runtime: a sqlx connection belongs to
the runtime whose reactor opened it.

`registry()` and `config()` below build the `Registry` and the `WorkerConfig`, as in the axum
program further down.

```rust
use std::thread::JoinHandle;

use library_jobs::{ExitReason, Worker};
use tokio_util::sync::CancellationToken;

fn spawn_jobs_runtime(url: String, stop: CancellationToken) -> std::io::Result<JoinHandle<anyhow::Result<()>>> {
    std::thread::Builder::new().name("jobs".into()).spawn(move || -> anyhow::Result<()> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("jobs-worker")
            .enable_all()
            .build()?;
        rt.block_on(async move {
            let pool = sqlx::postgres::PgPoolOptions::new().max_connections(8).connect(&url).await?;
            let worker = Worker::new(pool, registry()?, config())?;
            match worker.run(stop).await? {
                ExitReason::Shutdown => Ok(()),
                ExitReason::SelfFenced => anyhow::bail!("jobs worker fenced itself off; restart the process"),
            }
        })
    })
}
// ... serve HTTP on the main runtime; on shutdown: stop.cancel(); jobs_thread.join()
```

A separate process (`your-app worker`) isolates more: a job that leaks memory or crashes
cannot take the API with it, and workers scale and restart on their own. The library app runs
it that way by default ([docs/JOBS.md](../../docs/JOBS.md)).

**Connections.** Give the worker its own pool so job work never waits for the API's
connections. It holds one connection for LISTEN and, while leader, one for the advisory lock;
claims, heartbeats and handlers share the rest. `slots + 4` is a safe size.

## axum

`Enqueuer` is `Clone` and cheap: put it in your state. With `#[derive(FromRef)]` a handler
can ask for just the piece it needs. (The derive is behind axum's `macros` feature, which your
own `Cargo.toml` enables; `library-jobs` itself does not need it.)

The complete program is `crates/jobs/examples/axum_app.rs`, built by `cargo build -p library-jobs
--examples` and exercised by `crates/jobs/tests/axum_integration.rs`. It differs from the
snippets below only in names (a table called `example_users`, a `Greeter` standing in for the
`Mailer`) and in creating that table itself instead of running your migrations.

```rust
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use library_jobs::{EnqueueError, Enqueuer};
use sqlx::PgPool;

#[derive(Clone, FromRef)]
struct AppState {
    pool: PgPool,
    jobs: Enqueuer,
}

#[derive(serde::Deserialize)]
struct SignUp {
    email: String,
}

async fn sign_up(
    State(pool): State<PgPool>,
    State(jobs): State<Enqueuer>,
    Json(body): Json<SignUp>,
) -> Result<StatusCode, AppError> {
    let mut tx = pool.begin().await?;
    let id: Option<i64> =
        sqlx::query_scalar("INSERT INTO users (email) VALUES ($1) ON CONFLICT (email) DO NOTHING RETURNING id")
            .bind(&body.email)
            .fetch_optional(&mut *tx)
            .await?;
    // Returning drops `tx`: a rollback, and no job.
    let Some(id) = id else { return Ok(StatusCode::CONFLICT) };
    jobs.enqueue(&mut tx, &WelcomeEmail { user_id: id }).await?;
    tx.commit().await?; // the user and the job commit together; NOTIFY goes out now
    Ok(StatusCode::CREATED)
}

/// Both errors mean "could not save": a 500, logged, never the database's text to the client.
struct AppError(anyhow::Error);

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        Self(err.into())
    }
}

impl From<EnqueueError> for AppError {
    fn from(err: EnqueueError) -> Self {
        Self(err.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        tracing::error!(error = ?self.0, "request failed");
        StatusCode::INTERNAL_SERVER_ERROR.into_response()
    }
}
```

`EnqueueError::TooLarge` (arguments over 64 KB), `Options` (invalid `JobOptions`) and `Encode`
(the arguments do not serialize) are programming errors; `Db` is the database. A duplicate
unique key is not an error: `enqueue` returns `Enqueued::Duplicate(id)` and your transaction
stays usable.

If your handlers go through a repository layer (as the library app's do), give the
repository the `Enqueuer` and enqueue just before its `commit`, so the handler does not see a
transaction at all. See `apps/web/src/loans/postgres.rs`.

**Running the worker inside the server** shares one `CancellationToken` with axum's graceful
shutdown. When a signal arrives, HTTP drains (for a bounded time) while the worker finishes or
releases its jobs, and `main` waits for both. SIGTERM, which container runtimes send, is handled
like Ctrl-C:

```rust
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::routing::post;
use axum::Router;
use library_jobs::{Enqueuer, ExitReason, QueueConfig, Registry, Worker, WorkerConfig};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// How long the public server may spend draining in-flight requests after `stop` fires.
const DRAIN_BUDGET: Duration = Duration::from_secs(10);

fn registry() -> anyhow::Result<Registry> {
    Ok(Registry::new()
        .register::<WelcomeEmail>()
        .provide(Arc::new(Mailer::from_env()?)))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let url = std::env::args()
        .nth(1)
        .context("usage: axum_app <DATABASE_URL>")?;

    let pool = PgPoolOptions::new()
        .max_connections(10)
        .connect(&url)
        .await?;
    sqlx::migrate!().run(&pool).await?; // your schema
    library_jobs::migrate(&pool).await?;

    // The worker gets a pool of its own, so jobs never wait for the API's connections.
    let worker_pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await?;
    let worker = Worker::new(
        worker_pool,
        registry()?,
        WorkerConfig::new(QueueConfig::parse_list("email:4")?),
    )?;
    let internal = worker.router();

    let stop = CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown_signal().await;
            stop.cancel();
        }
    });
    let worker_task = tokio::spawn({
        let stop = stop.clone();
        async move {
            let result = worker.run(stop.clone()).await;
            // A fenced worker takes the HTTP server down with it, so the process can restart.
            stop.cancel();
            result
        }
    });

    // Whatever happens to HTTP, cancel `stop` and wait for the worker: it finishes its
    // jobs or hands them back to the queue instead of leaving them to lease expiry.
    let served = serve_http(internal, pool, stop.clone()).await;
    stop.cancel();
    let reason = worker_task.await??;
    served?;
    match reason {
        ExitReason::Shutdown => Ok(()),
        ExitReason::SelfFenced => {
            anyhow::bail!("the jobs worker fenced itself off; restart the process")
        }
    }
}

/// Both listeners, until `stop` fires; the public one then gets `DRAIN_BUDGET` to finish.
async fn serve_http(internal: Router, pool: PgPool, stop: CancellationToken) -> anyhow::Result<()> {
    // Internal endpoints on their own address, never on the public listener. Nothing
    // awaits this task: returning from `main` ends it.
    let private = tokio::net::TcpListener::bind("127.0.0.1:9300").await?;
    tokio::spawn(library_jobs::http::serve(private, internal, stop.clone()));

    let app = Router::new()
        .route("/signup", post(sign_up))
        .with_state(AppState {
            pool,
            jobs: Enqueuer::new(true),
        });
    let public = tokio::net::TcpListener::bind("127.0.0.1:3000").await?;
    tracing::info!("API on 127.0.0.1:3000, jobs endpoints on 127.0.0.1:9300; Ctrl-C to stop");
    let server = axum::serve(public, app).with_graceful_shutdown(stop.clone().cancelled_owned());
    // The graceful drain waits for in-flight requests and streams, without a limit.
    let mut server = std::pin::pin!(server.into_future());
    tokio::select! {
        served = &mut server => served?,
        () = async {
            stop.cancelled().await;
            tokio::time::sleep(DRAIN_BUDGET).await;
        } => tracing::warn!("HTTP drain budget spent; dropping the remaining connections"),
    }
    Ok(())
}

/// Resolves on Ctrl-C or, on Unix, SIGTERM, which is what container runtimes send.
async fn shutdown_signal() {
    let ctrl_c = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
```

**Bound the HTTP drain.** A graceful shutdown, whether `axum::serve(..).with_graceful_shutdown(..)`
or `library_jobs::http::serve`, returns only when the in-flight requests and streams have
finished (axum closes idle keep-alive connections at once), and it has no upper bound: a slow
download, an event stream or a hung handler holds the shutdown open, and the supervisor's SIGKILL
arrives before the worker has released its jobs. `serve_http` above races the public server
against `DRAIN_BUDGET` from the moment `stop` fires, and whatever happens it cancels `stop` and
waits for the worker. The internal server is spawned and never awaited, so returning from `main`
ends it; if you do await `http::serve`, give it the same `select!` with a timer. Keep the drain
budget inside the supervisor's grace period (see "Graceful shutdown" under [tokio](#tokio)).

**Mounting the internal router** in an existing app instead of a second listener works too
(`app.nest("/internal/jobs", worker.router())`), but then anyone who can reach your API can read
your queue metrics: put it behind your admin auth layer, or keep the separate address.

**Tower.** The crate adds no middleware. Enqueue is one `INSERT` (and a `pg_notify`) on your
own connection, inside your transaction; it does not need a layer.

## Other web frameworks

Everything outside `http` is framework-free: `Enqueuer` takes a connection, `Worker::run` is a
future, `Metrics::encode()` returns the Prometheus text, and `library_jobs::ready(&health, &pool,
stale_after)` answers readiness. Anything that runs on tokio can use them. These sketches are not
compiled; the axum example is.

| Framework | Enqueuer | Worker | Health and metrics |
|---|---|---|---|
| actix-web 4 | `web::Data<Enqueuer>` | A runtime of its own (below) | Two small handlers around `ready()` and `encode()` |
| Rocket 0.5 | `.manage(enqueuer)`, `&State<Enqueuer>` | `AdHoc::on_liftoff` spawns it; `AdHoc::on_shutdown` cancels the token | Two routes |
| Poem, Salvo, warp, hyper | Their state or data mechanism | `tokio::spawn`, as with axum | Their handlers, or the axum router on a second port |
| Loco | Its app context | A separate `worker` binary, or `tokio::spawn` in `after_routes` | The axum router (Loco is axum) |
| async-std, smol | Not supported: sqlx's Postgres driver here is built for tokio | | |

**actix-web.** `#[actix_web::main]` runs a single-threaded tokio runtime, and each HTTP worker
thread has its own. A worker spawned there shares one thread with the system arbiter. Start it
on [a runtime of its own](#tokio) before `HttpServer::run`, cancel its token after `run` returns,
and join the thread:

```rust
#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let stop = CancellationToken::new();
    let url = std::env::var("DATABASE_URL").map_err(std::io::Error::other)?;
    let jobs_thread = spawn_jobs_runtime(url.clone(), stop.clone())?; // under "A runtime of its own"
    let enqueuer = web::Data::new(Enqueuer::new(true));
    let pool = web::Data::new(PgPool::connect(&url).await.map_err(std::io::Error::other)?);
    HttpServer::new(move || App::new().app_data(enqueuer.clone()).app_data(pool.clone()).service(sign_up))
        .bind(("0.0.0.0", 3000))?
        .run()
        .await?;
    stop.cancel();
    let _ = jobs_thread.join();
    Ok(())
}

#[get("/readyz")]
async fn readyz(health: web::Data<Arc<Health>>, pool: web::Data<PgPool>) -> HttpResponse {
    match library_jobs::ready(&health, &pool, Duration::from_secs(60)).await {
        Ok(()) => HttpResponse::Ok().body("ready\n"),
        Err(not_ready) => HttpResponse::ServiceUnavailable().body(format!("{not_ready}\n")),
    }
}
```

## Testing your jobs

- **A handler alone:** `JobContext::detached(pool, id, attempt, kind, deps)` builds a context
  outside any worker; call `job.perform(ctx).await` and assert on the result. A handler that does
  not touch the database can take a lazy pool that never connects
  (`PgPoolOptions::new().connect_lazy("postgres://unused@127.0.0.1:1/x")`).
- **The enqueue:** drive your axum router with `tower::ServiceExt::oneshot`, then
  `SELECT kind, args FROM jobs`. Check that a refused request (409, 422) enqueued nothing.
  `crates/jobs/tests/axum_integration.rs` does this with the handler from the axum section.
- **End to end:** start a `Worker` with a short poll (`config.poll = 100 ms`) and a short
  `config.heartbeat` in the test, make the request, wait for the side effect, then cancel the
  token and await `run`; it returns `ExitReason::Shutdown`. A kind's lease must be at least three
  heartbeats, so a test kind with a 1 s lease needs a heartbeat of 300 ms or less.

Each test should use a fresh database; the crate's own tests create and drop one per test
(`crates/jobs/tests/common/mod.rs`).

## Features

| Feature | Default | Adds |
|---|---|---|
| `http` | yes | `http::router`, `http::serve` and `Worker::router()`: axum 0.8 handlers for `/metrics`, `/healthz`, `/readyz` |

With `default-features = false` the crate does not depend on axum; `Metrics::encode()` and
`ready()` remain.

## Guarantees and limits

- **At least once.** A job can run again after a crash between its side effect and the
  completion write. Make handlers idempotent, or give the receiver a stable id to de-duplicate on
  (`ctx.id()` is the same on every attempt; the library app's emails use it as `Message-ID`).
- **Never lost.** A job ends `succeeded`, `failed` (attempts or crashes used up, or a permanent
  error) or `cancelled`. Lease expiry counts as a crash; after `max_crashes` (2) the job fails
  instead of killing worker after worker.
- **Unique keys** de-duplicate while waiting or running, not after: a finished job's key can be
  used again. That is also why `ops::retry` of a failed or cancelled job returns `false` when
  another waiting or running job now holds its key. `enqueue_with` and
  `EnqueueOptions { replace: true, .. }` debounce: a waiting duplicate takes the new arguments,
  concurrency key, delay and priority (a running one is left alone), except that a duplicate
  waiting out a retry backoff keeps its later `run_at`.
- **No ordering** between jobs, even in one queue. Use `concurrency_key` to keep jobs with one key
  from running at the same time.
- **A database outage stops claims, not the process.** If heartbeats fail, handlers are
  cancelled one beat before the shortest lease can expire (the last good beat + the shortest
  lease - `heartbeat`; 40 s with the defaults) and the worker claims nothing while its last good
  beat is older than that. It keeps running and retrying, and picks up work again once a beat
  succeeds; it never exits for an outage. Only a deleted registry row ends it
  (`ExitReason::SelfFenced`). The leader prunes dead processes only after the database has been
  healthy for `prune_after`, so an outage does not make every live process look dead.
- **Throughput.** At least 1,000 no-op jobs per second with one worker on a 2-core box
  (`examples/throughput.rs`). Each job costs a claim, a completion write and a share of a heartbeat.
- **Not included:** cron expressions (recurring jobs use a fixed interval), job dependencies or
  workflows, a web dashboard (the `ops` module is the library behind a CLI), trace propagation,
  databases other than Postgres.

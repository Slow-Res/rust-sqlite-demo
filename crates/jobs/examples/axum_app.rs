//! The crate inside an axum app (README, "axum"): a sign-up handler enqueues a
//! job in its transaction, a worker runs in the same process, /metrics /healthz
//! /readyz are served on an internal address, and Ctrl-C drains HTTP, then the
//! worker. Use a scratch database:
//!
//!   cargo run -p library-jobs --example axum_app -- <DATABASE_URL>
//!   curl -i -H 'content-type: application/json' -d '{"email":"a@example.com"}' 127.0.0.1:3000/signup
//!   curl -s 127.0.0.1:9300/metrics | grep jobs_completed

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::{FromRef, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use library_jobs::{
    EnqueueError, Enqueuer, ExitReason, Job, JobContext, JobError, JobOptions, QueueConfig,
    Registry, Worker, WorkerConfig,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio_util::sync::CancellationToken;

/// Stands in for an SMTP client: shared services reach handlers through `ctx.require`.
struct Greeter {
    greeting: String,
}

#[derive(Serialize, Deserialize)]
struct WelcomeEmail {
    user_id: i64,
}

impl Job for WelcomeEmail {
    const KIND: &'static str = "example_welcome_email.v1";
    const QUEUE: &'static str = "email";

    fn options() -> JobOptions {
        JobOptions {
            deadline: Duration::from_secs(30),
            ..JobOptions::default()
        }
    }

    fn unique_key(&self) -> Option<String> {
        Some(self.user_id.to_string())
    }

    async fn perform(self, ctx: JobContext) -> Result<(), JobError> {
        let greeter = ctx.require::<Greeter>()?;
        let email: Option<String> =
            sqlx::query_scalar("SELECT email FROM example_users WHERE id = $1")
                .bind(self.user_id)
                .fetch_optional(ctx.pool())
                .await?;
        match email {
            Some(email) => tracing::info!(
                job_id = ctx.id(),
                attempt = ctx.attempt(),
                "{} {email}",
                greeter.greeting
            ),
            None => tracing::info!(
                job_id = ctx.id(),
                "user {} is gone; nothing to send",
                self.user_id
            ),
        }
        Ok(())
    }
}

#[derive(Clone, FromRef)]
struct AppState {
    pool: PgPool,
    jobs: Enqueuer,
}

#[derive(Deserialize)]
struct SignUp {
    email: String,
}

async fn sign_up(
    State(pool): State<PgPool>,
    State(jobs): State<Enqueuer>,
    Json(body): Json<SignUp>,
) -> Result<StatusCode, AppError> {
    let mut tx = pool.begin().await?;
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO example_users (email) VALUES ($1) ON CONFLICT (email) DO NOTHING RETURNING id",
    )
    .bind(&body.email)
    .fetch_optional(&mut *tx)
    .await?;
    // Returning drops `tx`: a rollback, and no job.
    let Some(id) = id else {
        return Ok(StatusCode::CONFLICT);
    };
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

fn registry() -> Registry {
    Registry::new()
        .register::<WelcomeEmail>()
        .provide(Arc::new(Greeter {
            greeting: "welcome,".to_owned(),
        }))
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
    // Stands in for your own migrations.
    sqlx::query("CREATE TABLE IF NOT EXISTS example_users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email TEXT NOT NULL UNIQUE)")
        .execute(&pool)
        .await?;
    library_jobs::migrate(&pool).await?;

    // The worker gets a pool of its own, so jobs never wait for the API's connections.
    let worker_pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await?;
    let worker = Worker::new(
        worker_pool,
        registry(),
        WorkerConfig::new(QueueConfig::parse_list("email:4")?),
    )?;
    let internal = worker.router();

    let stop = CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            let _ = tokio::signal::ctrl_c().await;
            stop.cancel();
        }
    });
    let worker_task = tokio::spawn(worker.run(stop.clone()));

    // Internal endpoints on their own address, never on the public listener.
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
    axum::serve(public, app)
        .with_graceful_shutdown(stop.clone().cancelled_owned())
        .await?;

    // HTTP has drained; now running jobs finish or go back to the queue.
    match worker_task.await?? {
        ExitReason::Shutdown => Ok(()),
        ExitReason::SelfFenced => {
            anyhow::bail!("the jobs worker fenced itself off; restart the process")
        }
    }
}

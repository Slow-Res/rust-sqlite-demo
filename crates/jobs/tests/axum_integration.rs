//! The crate used the way the README's axum section shows: a handler enqueues
//! in its transaction, and a worker in the same process runs the job.
//!
//! The handler repeats the README's shape on purpose instead of sharing code
//! with `examples/axum_app.rs` (tests cannot import examples): the point is to
//! pin the documented pattern.
#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRef, State};
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use common::*;
use library_jobs::{Enqueuer, ExitReason};
use sqlx::PgPool;
use tower::ServiceExt;

#[derive(Clone, FromRef)]
struct AppState {
    pool: PgPool,
    jobs: Enqueuer,
}

#[derive(serde::Deserialize)]
struct SignUp {
    email: String,
}

/// The README's handler. An address ending in `@rollback.test` stands in for a
/// failure after the enqueue: the transaction is dropped, user and job vanish together.
async fn sign_up(
    State(pool): State<PgPool>,
    State(jobs): State<Enqueuer>,
    Json(body): Json<SignUp>,
) -> StatusCode {
    let mut tx = pool.begin().await.unwrap();
    let id: Option<i64> = sqlx::query_scalar(
        "INSERT INTO users (email) VALUES ($1) ON CONFLICT (email) DO NOTHING RETURNING id",
    )
    .bind(&body.email)
    .fetch_optional(&mut *tx)
    .await
    .unwrap();
    let Some(id) = id else {
        return StatusCode::CONFLICT;
    };
    jobs.enqueue(&mut tx, &Noop { n: id }).await.unwrap();
    if body.email.ends_with("@rollback.test") {
        return StatusCode::UNPROCESSABLE_ENTITY;
    }
    tx.commit().await.unwrap();
    StatusCode::CREATED
}

fn app(pool: PgPool) -> Router {
    Router::new()
        .route("/signup", post(sign_up))
        .with_state(AppState {
            pool,
            jobs: Enqueuer::new(true),
        })
}

async fn post_signup(app: &Router, email: &str) -> StatusCode {
    let req = Request::post("/signup")
        .header("content-type", "application/json")
        .body(Body::from(format!(r#"{{"email":"{email}"}}"#)))
        .unwrap();
    app.clone().oneshot(req).await.unwrap().status()
}

async fn users_table(pool: &PgPool) {
    sqlx::query("CREATE TABLE users (id BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY, email TEXT NOT NULL UNIQUE)")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_enqueues_inside_its_transaction() {
    with_db(|pool| async move {
        users_table(&pool).await;
        let app = app(pool.clone());
        assert_eq!(
            post_signup(&app, "a@example.test").await,
            StatusCode::CREATED
        );
        assert_eq!(
            post_signup(&app, "a@example.test").await,
            StatusCode::CONFLICT
        );
        assert_eq!(
            post_signup(&app, "b@rollback.test").await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(count(&pool, "SELECT count(*) FROM users").await, 1);
        assert_eq!(
            count(&pool, "SELECT count(*) FROM jobs").await,
            1,
            "one job, for the one committed user"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_in_the_same_process_runs_the_job_and_stops_cleanly() {
    with_db(|pool| async move {
        users_table(&pool).await;
        let w = start(&pool, fast_config("default:2"));
        let app = app(pool.clone());
        assert_eq!(
            post_signup(&app, "c@example.test").await,
            StatusCode::CREATED
        );
        eventually(Duration::from_secs(5), "the job ran", || async {
            count(&pool, "SELECT count(*) FROM test_runs").await == 1
        })
        .await;
        assert_eq!(w.shutdown().await, ExitReason::Shutdown);
    })
    .await;
}

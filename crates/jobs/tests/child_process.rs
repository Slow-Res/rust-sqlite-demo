//! A real SIGKILL: this test binary re-runs itself as a worker process.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::process::{Command, Stdio};
use std::time::Duration;

use common::*;
use library_jobs::Worker;
use tokio_util::sync::CancellationToken;

/// Does nothing unless JOBS_CHILD_DB_URL is set; then it *is* the worker the
/// parent test kills.
#[tokio::test(flavor = "multi_thread")]
async fn child_worker_entry() {
    let Ok(url) = std::env::var("JOBS_CHILD_DB_URL") else {
        return;
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
        .unwrap();
    let mut config = fast_config("default:2");
    config.maintenance.enabled = false;
    Worker::new(pool, registry(), config)
        .unwrap()
        .run(CancellationToken::new())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sigkill_mid_job_is_recovered_by_another_worker() {
    with_db_url(|pool, url| async move {
        let id = enqueue(&pool, &SleepOnce { ms: 60_000 }).await;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "child_worker_entry",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("JOBS_CHILD_DB_URL", &url)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        eventually(Duration::from_secs(20), "child claimed the job", || async {
            job_state(&pool, id).await == "running"
        })
        .await;
        child.kill().unwrap(); // SIGKILL: no cleanup, no deregistration
        child.wait().unwrap();
        let w = start(&pool, fast_config("default:2"));
        eventually(
            Duration::from_secs(15),
            "recovered and succeeded",
            || async { job_state(&pool, id).await == "succeeded" },
        )
        .await;
        let (attempt, crashes, cause): (i32, i32, String) =
            sqlx::query_as("SELECT attempt, crashes, errors->0->>'cause' FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((attempt, crashes, cause.as_str()), (2, 1, "process_lost"));
        assert_eq!(
            count(&pool, "SELECT count(*) FROM test_runs WHERE attempt = 2").await,
            1
        );
        w.shutdown().await;
    })
    .await;
}

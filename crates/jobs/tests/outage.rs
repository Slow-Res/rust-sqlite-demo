//! Pauses a dedicated Postgres container for 20 s mid-run (spec §20 "DB outage").
//! Run with: cargo test -p library-jobs --test outage -- --ignored
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::process::Command;
use std::time::Duration;

use common::*;

const NAME: &str = "jobs-outage-test-pg";
const URL: &str = "postgres://postgres:test@127.0.0.1:55433/postgres"; // local only

fn docker(args: &[&str]) {
    let ok = Command::new("docker")
        .args(args)
        .status()
        .unwrap()
        .success();
    assert!(ok, "docker {args:?} failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Docker; starts, pauses and removes its own container"]
async fn workers_survive_a_database_outage() {
    let _ = Command::new("docker").args(["rm", "-f", NAME]).status();
    docker(&[
        "run",
        "-d",
        "--name",
        NAME,
        "-e",
        "POSTGRES_PASSWORD=test", // local only: throwaway container bound to 127.0.0.1
        "-p",
        "127.0.0.1:55433:5432",
        "--tmpfs",
        "/var/lib/postgresql",
        "postgres:18.6",
    ]);
    let result = tokio::spawn(async {
        let pool = loop {
            match sqlx::postgres::PgPoolOptions::new().max_connections(20).acquire_timeout(Duration::from_secs(60)).connect(URL).await {
                Ok(p) => break p,
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        library_jobs::migrate(&pool).await.unwrap();
        sqlx::query("CREATE TABLE test_runs (n BIGINT NOT NULL, attempt INT NOT NULL, at TIMESTAMPTZ NOT NULL DEFAULT now())")
            .execute(&pool).await.unwrap();
        for _ in 0..200 { enqueue(&pool, &Sleepy { ms: 200, honour_cancel: true }).await; }
        let a = start(&pool, fast_config("default:4"));
        let b = start(&pool, fast_config("default:4"));
        tokio::time::sleep(Duration::from_secs(1)).await;
        docker(&["pause", NAME]);
        tokio::time::sleep(Duration::from_secs(20)).await;
        docker(&["unpause", NAME]);
        eventually(Duration::from_secs(120), "all 200 succeeded", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'succeeded'").await == 200
        }).await;
        assert!(!a.handle.is_finished() && !b.handle.is_finished(), "a worker exited during the outage");
        a.shutdown().await;
        b.shutdown().await;
    }).await;
    let _ = Command::new("docker").args(["rm", "-f", NAME]).status();
    if let Err(err) = result {
        std::panic::resume_unwind(err.into_panic());
    }
}

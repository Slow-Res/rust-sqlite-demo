//! The `worker` and `jobs` subcommands of the real binary, against a fresh database.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A child process that is killed when the test ends, so a failing assertion
/// cannot leave a server running against a dropped database.
struct KillOnDrop(Child);

impl std::ops::Deref for KillOnDrop {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.0
    }
}

impl std::ops::DerefMut for KillOnDrop {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.0
    }
}

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin(url: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_library-web"));
    // Outside the repo, so no .env file is picked up.
    cmd.current_dir(std::env::temp_dir())
        .env("DATABASE_URL", url)
        .env("JOBS_ENABLED", "true")
        .env("RUST_LOG", "warn")
        .env_remove("JOBS_SMTP_URL")
        .env_remove("JOBS_WEBHOOK_ENDPOINTS");
    cmd
}

fn run(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn wait_exit(child: &mut Child, within: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        assert!(
            start.elapsed() < within,
            "the worker did not exit within {within:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_becomes_ready_and_stops_cleanly_on_sigterm() {
    common::on_postgres_url(|db, url| async move {
        let library_web::db::Database::Postgres(pool) = db else {
            panic!("postgres only")
        };
        let port = free_port();
        let mut child = KillOnDrop(
            bin(&url)
                .arg("worker")
                .env("JOBS_METRICS_ADDR", format!("127.0.0.1:{port}"))
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let ready = format!("http://127.0.0.1:{port}/readyz");
        let start = Instant::now();
        while !reqwest::get(&ready)
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "not ready in 30 s"
            );
            assert!(
                child.try_wait().unwrap().is_none(),
                "the worker exited early"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let metrics = reqwest::get(format!("http://127.0.0.1:{port}/metrics"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(metrics.contains("jobs_leader"), "{metrics}");

        let processes = run(bin(&url).args(["jobs", "processes"]));
        assert!(String::from_utf8_lossy(&processes.stdout).contains("live"));
        let status = run(bin(&url).args(["jobs", "status"]));
        assert!(String::from_utf8_lossy(&status.stdout).contains("no waiting"));

        run(Command::new("kill").args(["-TERM", &child.id().to_string()]));
        assert!(
            wait_exit(&mut child, Duration::from_secs(30))
                .await
                .success()
        );
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM job_processes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "deregistered on shutdown");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_refuses_to_start_without_jobs_enabled() {
    common::on_postgres_url(|_db, url| async move {
        let out = bin(&url)
            .arg("worker")
            .env("JOBS_ENABLED", "false")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("JOBS_ENABLED"));
    })
    .await;
}

/// `serve` with an embedded worker, listening on a free port.
fn embedded_serve(url: &str) -> KillOnDrop {
    KillOnDrop(
        bin(url)
            .arg("serve")
            .env("JOBS_EMBEDDED", "true")
            .env("BIND_ADDR", "127.0.0.1:0")
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

async fn wait_for_embedded_row(pool: &sqlx::PgPool, child: &mut Child) {
    let start = Instant::now();
    loop {
        // An error means the jobs migration has not run yet.
        let rows: i64 =
            sqlx::query_scalar("SELECT count(*) FROM job_processes WHERE kind = 'embedded'")
                .fetch_one(pool)
                .await
                .unwrap_or(0);
        if rows == 1 {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "no embedded worker registered in 30 s"
        );
        assert!(child.try_wait().unwrap().is_none(), "serve exited early");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn embedded_worker_stops_cleanly_with_the_server() {
    common::on_postgres_url(|db, url| async move {
        let library_web::db::Database::Postgres(pool) = db else {
            panic!("postgres only")
        };
        let mut child = embedded_serve(&url);
        wait_for_embedded_row(&pool, &mut child).await;

        run(Command::new("kill").args(["-TERM", &child.id().to_string()]));
        assert!(
            wait_exit(&mut child, Duration::from_secs(30))
                .await
                .success(),
            "a requested stop is not a failure"
        );
        let left: i64 = sqlx::query_scalar("SELECT count(*) FROM job_processes")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(left, 0, "deregistered on shutdown");
    })
    .await;
}

/// Spec §8.2: a fenced embedded worker takes the server down with a non-zero
/// exit, so the supervisor restarts it. The fence is noticed on the next
/// heartbeat (20 s period), so this test takes up to that long.
#[tokio::test(flavor = "multi_thread")]
async fn fenced_embedded_worker_ends_serve_with_an_error() {
    common::on_postgres_url(|db, url| async move {
        let library_web::db::Database::Postgres(pool) = db else {
            panic!("postgres only")
        };
        let mut child = embedded_serve(&url);
        wait_for_embedded_row(&pool, &mut child).await;

        sqlx::query("DELETE FROM job_processes WHERE kind = 'embedded'")
            .execute(&pool)
            .await
            .unwrap();
        let status = wait_exit(&mut child, Duration::from_secs(45)).await;
        assert!(
            !status.success(),
            "serve must exit non-zero when its worker is fenced"
        );
    })
    .await;
}

/// Spec §11.4: a stuck query must not hold a worker connection forever.
#[tokio::test(flavor = "multi_thread")]
async fn worker_pool_connections_have_a_statement_timeout() {
    common::on_postgres_url(|_db, url| async move {
        let pool = library_web::jobs::connect(&url, 2).await.unwrap();
        let timeout: String = sqlx::query_scalar("SHOW statement_timeout")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(timeout, "30s");
        pool.close().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_refuses_embedded_without_jobs_enabled() {
    common::on_postgres_url(|_db, url| async move {
        let out = bin(&url)
            .arg("serve")
            .env("JOBS_ENABLED", "false")
            .env("JOBS_EMBEDDED", "true")
            .env("BIND_ADDR", "127.0.0.1:0")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("JOBS_EMBEDDED"));
    })
    .await;
}

/// A taken port must fail `serve` before any embedded worker is started.
#[tokio::test(flavor = "multi_thread")]
async fn serve_bind_failure_leaves_no_embedded_worker() {
    common::on_postgres_url(|db, url| async move {
        let library_web::db::Database::Postgres(pool) = db else {
            panic!("postgres only")
        };
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let out = bin(&url)
            .arg("serve")
            .env("JOBS_EMBEDDED", "true")
            .env("BIND_ADDR", taken.local_addr().unwrap().to_string())
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("binding"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM job_processes")
            .fetch_one(&pool)
            .await
            .unwrap_or(0);
        assert_eq!(rows, 0, "no worker was registered");
    })
    .await;
}

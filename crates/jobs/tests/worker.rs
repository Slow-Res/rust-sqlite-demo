#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::worker::ExitReason;

const T: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn every_job_runs_exactly_once_across_four_workers() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             SELECT 'test_noop.v1', 'default', jsonb_build_object('n', i), 10, 2, 900, 1
             FROM generate_series(1, 5000) i",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut config = fast_config("default:8");
        config.maintenance.enabled = false;
        let workers: Vec<TestWorker> = (0..4).map(|_| start(&pool, config.clone())).collect();
        eventually(Duration::from_secs(90), "all 5000 succeeded", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'succeeded'").await == 5000
        })
        .await;
        for w in workers {
            assert_eq!(w.shutdown().await, ExitReason::Shutdown);
        }
        assert_eq!(count(&pool, "SELECT count(*) FROM test_runs").await, 5000);
        assert_eq!(
            count(&pool, "SELECT count(DISTINCT n) FROM test_runs").await,
            5000
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn notify_wakes_an_idle_worker_long_before_its_poll() {
    with_db(|pool| async move {
        let mut config = fast_config("default:2");
        config.poll = Duration::from_secs(30);
        let w = start(&pool, config);
        tokio::time::sleep(Duration::from_millis(500)).await; // let it LISTEN and go idle
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        eventually(Duration::from_secs(2), "woken by NOTIFY", || async {
            job_state(&pool, id).await == "succeeded"
        })
        .await;
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_listen_the_poll_still_finds_jobs() {
    with_db(|pool| async move {
        let mut config = fast_config("default:2");
        config.listen = false;
        let w = start(&pool, config);
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        eventually(Duration::from_secs(2), "found by polling", || async {
            job_state(&pool, id).await == "succeeded"
        })
        .await;
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cancel_request_stops_the_handler_within_one_heartbeat() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let id = enqueue(
            &pool,
            &Sleepy {
                ms: 30_000,
                honour_cancel: true,
            },
        )
        .await;
        eventually(T, "running", || async {
            job_state(&pool, id).await == "running"
        })
        .await;
        sqlx::query("UPDATE jobs SET cancel_requested = true WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        eventually(Duration::from_secs(1), "cancelled", || async {
            job_state(&pool, id).await == "cancelled"
        })
        .await;
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_lease_cancels_the_handler_and_writes_nothing() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let id = enqueue(
            &pool,
            &Sleepy {
                ms: 30_000,
                honour_cancel: true,
            },
        )
        .await;
        eventually(T, "running", || async {
            job_state(&pool, id).await == "running"
        })
        .await;
        sqlx::query("UPDATE jobs SET locked_by = -1 WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        eventually(
            Duration::from_secs(1),
            "handler saw cancellation",
            || async { count(&pool, "SELECT count(*) FROM test_runs WHERE n = -1").await == 1 },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let holder: i64 = sqlx::query_scalar("SELECT locked_by FROM jobs WHERE id = $1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            (job_state(&pool, id).await.as_str(), holder),
            ("running", -1)
        );
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_deleted_registry_row_makes_the_worker_fence_itself() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        eventually(T, "registered", || async {
            count(&pool, "SELECT count(*) FROM job_processes").await == 1
        })
        .await;
        sqlx::query("DELETE FROM job_processes")
            .execute(&pool)
            .await
            .unwrap();
        let reason = tokio::time::timeout(Duration::from_secs(2), w.handle)
            .await
            .expect("exits")
            .unwrap()
            .unwrap();
        assert_eq!(reason, ExitReason::SelfFenced);
        assert!(w.health.is_fenced());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn graceful_shutdown_finishes_short_jobs_and_releases_the_rest() {
    with_db(|pool| async move {
        let mut config = fast_config("default:4");
        config.shutdown_timeout = Duration::from_secs(1);
        let w = start(&pool, config);
        let short1 = enqueue(
            &pool,
            &Sleepy {
                ms: 300,
                honour_cancel: false,
            },
        )
        .await;
        let short2 = enqueue(
            &pool,
            &Sleepy {
                ms: 300,
                honour_cancel: false,
            },
        )
        .await;
        let long = enqueue(
            &pool,
            &Sleepy {
                ms: 30_000,
                honour_cancel: true,
            },
        )
        .await;
        eventually(T, "all running", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'running'").await == 3
        })
        .await;
        assert_eq!(w.shutdown().await, ExitReason::Shutdown);
        assert_eq!(job_state(&pool, short1).await, "succeeded");
        assert_eq!(job_state(&pool, short2).await, "succeeded");
        let (state, attempt, cause): (String, i32, String) =
            sqlx::query_as("SELECT state, attempt, errors->-1->>'cause' FROM jobs WHERE id = $1")
                .bind(long)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            (state.as_str(), attempt, cause.as_str()),
            ("available", 0, "released_on_shutdown")
        );
        assert_eq!(
            count(&pool, "SELECT count(*) FROM job_processes").await,
            0,
            "deregistered"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hung_handler_hits_deadline_and_worker_keeps_going() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let hung = enqueue(&pool, &ShortDeadline { ms: 3_000 }).await;
        let mut others = Vec::new();
        for n in 0..20 {
            others.push(enqueue(&pool, &Noop { n }).await);
        }
        eventually(
            Duration::from_secs(5),
            "others done while one hangs",
            || async {
                count(
                    &pool,
                    "SELECT count(*) FROM jobs WHERE kind = 'test_noop.v1' AND state = 'succeeded'",
                )
                .await
                    == 20
            },
        )
        .await;
        eventually(Duration::from_secs(3), "deadline recorded", || async {
            let cause: Option<String> =
                sqlx::query_scalar("SELECT errors->0->>'cause' FROM jobs WHERE id = $1")
                    .bind(hung)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            cause.as_deref() == Some("deadline_exceeded")
        })
        .await;
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_claims_only_its_queues_and_records_its_kinds() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("other:2"));
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(job_state(&pool, id).await, "available");
        let knows: bool =
            sqlx::query_scalar("SELECT config->'kinds' ? 'test_noop.v1' FROM job_processes")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(knows);
        w.shutdown().await;
    })
    .await;
}

/// The value of a metric line like `jobs_running{queue="default"}`, 0 when absent.
fn metric(text: &str, series: &str) -> i64 {
    text.lines()
        .find_map(|l| l.strip_prefix(series).and_then(|v| v.trim().parse().ok()))
        .unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread")]
async fn metrics_count_claimed_and_completed_jobs_and_running_returns_to_zero() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        for n in 0..5 {
            enqueue(&pool, &Noop { n }).await;
        }
        eventually(T, "all succeeded", || async {
            count(&pool, "SELECT count(*) FROM jobs WHERE state = 'succeeded'").await == 5
        })
        .await;
        // The gauge drops when the job task ends, just after its outcome is written.
        eventually(T, "running gauge is back to 0", || async {
            w.metrics
                .encode()
                .contains(r#"jobs_running{queue="default"} 0"#)
        })
        .await;
        let text = w.metrics.encode();
        assert_eq!(
            metric(&text, r#"jobs_claimed_total{queue="default"} "#),
            5,
            "{text}"
        );
        assert_eq!(
            metric(
                &text,
                r#"jobs_completed_total{kind="test_noop.v1",outcome="succeeded"} "#
            ),
            5,
            "{text}"
        );
        assert_eq!(
            metric(&text, r#"jobs_slots{queue="default"} "#),
            2,
            "{text}"
        );
        w.shutdown().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_handler_that_panics_while_being_built_is_recorded_and_leaks_nothing() {
    with_db(|pool| async move {
        let w = start(&pool, fast_config("default:2"));
        let id = enqueue(&pool, &Explodes { n: 1 }).await;
        eventually(T, "failed", || async {
            job_state(&pool, id).await == "failed"
        })
        .await;
        let cause: String =
            sqlx::query_scalar("SELECT errors->-1->>'cause' FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cause, "panic");
        eventually(T, "running gauge is back to 0", || async {
            w.metrics
                .encode()
                .contains(r#"jobs_running{queue="default"} 0"#)
        })
        .await;
        // The slot is free again: the worker keeps serving jobs.
        let ok = enqueue(&pool, &Noop { n: 2 }).await;
        eventually(T, "next job runs", || async {
            job_state(&pool, ok).await == "succeeded"
        })
        .await;
        w.shutdown().await;
    })
    .await;
}

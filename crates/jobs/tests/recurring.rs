#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::maintenance::LEADER_LOCK;
use library_jobs::store::maintenance::delete_finished;

#[tokio::test(flavor = "multi_thread")]
async fn each_tick_is_enqueued_once_across_leader_handovers() {
    with_db(|pool| async move {
        let reg = || registry().recurring::<Noop>("tick", Duration::from_secs(1), || Noop { n: 0 });
        let ws: Vec<TestWorker> = (0..3).map(|_| start_with(&pool, reg(), fast_config("default:2"))).collect();
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(1200)).await;
            // Force a hand-over by killing the leader's lock connection.
            sqlx::query(
                "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
                   AND ((classid::bigint << 32) | objid::bigint) = $1",
            ).bind(LEADER_LOCK).execute(&pool).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        for w in ws { w.shutdown().await; }
        let runs = count(&pool, "SELECT count(*) FROM job_recurring_runs WHERE task = 'tick'").await;
        let jobs = count(&pool, "SELECT count(*) FROM jobs WHERE kind = 'test_noop.v1'").await;
        let distinct = count(&pool, "SELECT count(DISTINCT run_at) FROM job_recurring_runs").await;
        assert_eq!(runs, jobs, "one job per recorded tick");
        assert_eq!(runs, distinct);
        assert!((3..=6).contains(&runs), "about one tick per second over ~4.8 s, got {runs}");
    }).await;
}

#[tokio::test]
async fn retention_deletes_only_rows_past_their_window_in_batches() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, state, max_attempts, max_crashes, deadline_s, lease_s, finished_at)
             SELECT 'k', 'q', '{}', s.state, 10, 2, 900, 60, now() - s.age
             FROM (VALUES ('succeeded', interval '2 days', 2500), ('succeeded', interval '1 minute', 10),
                          ('failed', interval '20 days', 5), ('failed', interval '1 day', 5),
                          ('cancelled', interval '20 days', 3)) AS s(state, age, n),
                  LATERAL generate_series(1, s.n)",
        ).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO job_recurring_runs (task, run_at, created_at) VALUES ('old', now(), now() - interval '8 days'), ('new', now(), now())")
            .execute(&pool).await.unwrap();
        let deleted = delete_finished(&pool, Duration::from_secs(86_400), Duration::from_secs(14 * 86_400), 1000).await.unwrap();
        assert_eq!(deleted, 2500 + 5 + 3);
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 15);
        assert_eq!(count(&pool, "SELECT count(*) FROM job_recurring_runs").await, 1);
    }).await;
}

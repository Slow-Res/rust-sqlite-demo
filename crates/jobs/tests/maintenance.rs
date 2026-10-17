#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::maintenance::LEADER_LOCK;
use library_jobs::store::maintenance::{prune, reap};
use sqlx::PgPool;

async fn expire(pool: &PgPool, id: i64) {
    sqlx::query("UPDATE jobs SET locked_until = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

async fn row(pool: &PgPool, id: i64) -> (String, i32, String) {
    sqlx::query_as("SELECT state, crashes, errors->-1->>'cause' FROM jobs WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn reaper_requeues_expired_leases_with_the_right_cause() {
    with_db(|pool| async move {
        let live = register_process(&pool, "live").await;
        let a = insert_running(&pool, live, "k", 60, 900).await;
        let b = insert_running(&pool, -7, "k", 60, 900).await; // holder has no registry row
        expire(&pool, a).await;
        expire(&pool, b).await;
        let reaped = reap(&pool).await.unwrap();
        assert_eq!(reaped.len(), 2);
        assert_eq!(
            row(&pool, a).await,
            ("available".into(), 1, "lease_expired".into())
        );
        assert_eq!(
            row(&pool, b).await,
            ("available".into(), 1, "process_lost".into())
        );
        assert!(
            reap(&pool).await.unwrap().is_empty(),
            "nothing left to reap"
        );
    })
    .await;
}

#[tokio::test]
async fn poison_jobs_fail_after_max_crashes_and_attempts_are_respected() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = insert_running(&pool, me, "k", 60, 900).await; // max_crashes 2
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, id).await.0, "available");
        force_running(&pool, id, me).await;
        expire(&pool, id).await;
        reap(&pool).await.unwrap();
        assert_eq!(
            row(&pool, id).await,
            ("failed".into(), 2, "lease_expired".into())
        );

        let last = insert_running(&pool, me, "k", 60, 900).await;
        sqlx::query("UPDATE jobs SET max_attempts = 1, max_crashes = 5 WHERE id = $1")
            .bind(last)
            .execute(&pool)
            .await
            .unwrap();
        expire(&pool, last).await;
        reap(&pool).await.unwrap();
        assert_eq!(row(&pool, last).await.0, "failed", "no attempts left");
    })
    .await;
}

#[tokio::test]
async fn pruner_removes_silent_processes_and_frees_their_jobs() {
    with_db(|pool| async move {
        let dead = register_process(&pool, "dead").await;
        let alive = register_process(&pool, "alive").await;
        let j = insert_running(&pool, dead, "k", 60, 900).await;
        let keep = insert_running(&pool, alive, "k", 60, 900).await;
        sqlx::query(
            "UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(dead)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            prune(&pool, Duration::from_secs(180)).await.unwrap(),
            (1, 1)
        );
        reap(&pool).await.unwrap();
        assert_eq!(
            row(&pool, j).await,
            ("available".into(), 1, "process_lost".into())
        );
        assert_eq!(job_state(&pool, keep).await, "running");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dead_process_job_is_recovered_by_a_live_worker() {
    with_db(|pool| async move {
        // What `kill -9` leaves behind: a registry row that stops beating and a running job.
        let dead = register_process(&pool, "dead").await;
        let id = enqueue(&pool, &SleepOnce { ms: 60_000 }).await;
        force_running(&pool, id, dead).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 hour'")
            .execute(&pool)
            .await
            .unwrap();
        let w = start(&pool, fast_config("default:2"));
        eventually(Duration::from_secs(10), "re-run and succeeded", || async {
            job_state(&pool, id).await == "succeeded"
        })
        .await;
        let (crashes, cause): (i32, String) =
            sqlx::query_as("SELECT crashes, errors->0->>'cause' FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((crashes, cause.as_str()), (1, "process_lost"));
        w.shutdown().await;
    })
    .await;
}

async fn leaders(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
           AND ((classid::bigint << 32) | objid::bigint) = $1",
    )
    .bind(LEADER_LOCK)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn exactly_one_leader_and_failover_when_its_connection_dies() {
    with_db(|pool| async move {
        let a = start(&pool, fast_config("default:1"));
        let b = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "a leader", || async { leaders(&pool).await == 1 }).await;
        for _ in 0..20 {
            assert!(leaders(&pool).await <= 1);
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        sqlx::query(
            "SELECT pg_terminate_backend(pid) FROM pg_locks WHERE locktype = 'advisory' AND granted AND objsubid = 1
           AND database = (SELECT oid FROM pg_database WHERE datname = current_database())
               AND ((classid::bigint << 32) | objid::bigint) = $1",
        ).bind(LEADER_LOCK).execute(&pool).await.unwrap();
        // A lease that expires now must still be reaped by the new leader.
        let me = register_process(&pool, "victim").await;
        let id = insert_running(&pool, me, "k", 60, 900).await;
        expire(&pool, id).await;
        eventually(Duration::from_secs(5), "reaped after failover", || async { job_state(&pool, id).await == "available" }).await;
        // The old leader keeps working until its next connection check (at most one
        // `leader_retry`), so the reap can precede the new election: wait for it.
        eventually(Duration::from_secs(5), "a new leader", || async { leaders(&pool).await == 1 }).await;
        a.shutdown().await;
        b.shutdown().await;
    }).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_leader_exports_queue_gauges() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let opts = library_jobs::EnqueueOptions {
            delay: Some(Duration::from_secs(3600)),
            ..Default::default()
        };
        for n in 0..2 {
            library_jobs::Enqueuer::new(false)
                .enqueue_with(&mut conn, &Noop { n }, opts.clone())
                .await
                .unwrap();
        }
        let w = start(&pool, fast_config("default:1"));
        eventually(Duration::from_secs(5), "gauges", || async {
            w.metrics
                .encode()
                .contains(r#"jobs_scheduled{queue="default"} 2"#)
        })
        .await;
        assert!(w.metrics.encode().contains("jobs_leader 1"));
        w.shutdown().await;
    })
    .await;
}

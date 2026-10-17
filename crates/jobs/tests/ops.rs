#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::ops::{self, CancelOutcome, ListFilter};

#[tokio::test]
async fn status_counts_queues_and_flags_orphan_kinds_and_pauses() {
    with_db(|pool| async move {
        enqueue(&pool, &Noop { n: 1 }).await;
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             VALUES ('ghost.v1', 'default', '{}', 1, 1, 60, 60)",
        ).execute(&pool).await.unwrap();
        let p = register_process(&pool, "w").await;
        sqlx::query("UPDATE job_processes SET config = '{\"queues\": [], \"kinds\": [\"test_noop.v1\"]}' WHERE id = $1")
            .bind(p).execute(&pool).await.unwrap();
        ops::pause(&pool, "default", "tester", Some("maintenance")).await.unwrap();
        let s = ops::status(&pool).await.unwrap();
        let q = &s.queues[0];
        assert_eq!((q.queue.as_str(), q.available, q.paused), ("default", 2, true));
        assert_eq!(s.orphan_kinds, vec!["ghost.v1".to_owned()]);
        assert_eq!(s.processes.len(), 1);
        assert!(ops::resume(&pool, "default").await.unwrap());
        assert!(!ops::status(&pool).await.unwrap().queues[0].paused);
    }).await;
}

#[tokio::test]
async fn list_show_retry_discard_and_cancel() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let waiting = enqueue(&pool, &Noop { n: 1 }).await;
        let running = enqueue(&pool, &Noop { n: 2 }).await;
        force_running(&pool, running, me).await;
        let failed = enqueue(&pool, &Noop { n: 3 }).await;
        sqlx::query(
            "UPDATE jobs SET state = 'failed', finished_at = now(), attempt = 10, crashes = 1,
                     errors = '[{\"cause\": \"error\", \"message\": \"boom\"}]' WHERE id = $1",
        )
        .bind(failed)
        .execute(&pool)
        .await
        .unwrap();

        let all = ops::list(
            &pool,
            &ListFilter {
                state: None,
                queue: None,
                kind: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            all.iter().map(|j| j.id).collect::<Vec<_>>(),
            vec![failed, running, waiting],
            "newest first"
        );
        let only_failed = ops::list(
            &pool,
            &ListFilter {
                state: Some("failed".into()),
                queue: None,
                kind: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(only_failed[0].last_error.as_deref(), Some("boom"));
        let detail = ops::show(&pool, running).await.unwrap().unwrap();
        assert_eq!(detail.locked_by, Some(me));

        assert!(ops::retry(&pool, failed).await.unwrap());
        let (state, attempt, crashes): (String, i32, i32) =
            sqlx::query_as("SELECT state, attempt, crashes FROM jobs WHERE id = $1")
                .bind(failed)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((state.as_str(), attempt, crashes), ("available", 0, 0));
        assert!(
            !ops::retry(&pool, running).await.unwrap(),
            "running jobs are not retried"
        );

        assert_eq!(
            ops::cancel(&pool, running).await.unwrap(),
            CancelOutcome::Requested
        );
        assert_eq!(
            ops::cancel(&pool, waiting).await.unwrap(),
            CancelOutcome::Cancelled
        );
        assert_eq!(
            ops::cancel(&pool, waiting).await.unwrap(),
            CancelOutcome::AlreadyFinished
        );
        assert_eq!(
            ops::cancel(&pool, 999_999).await.unwrap(),
            CancelOutcome::NotFound
        );
        assert!(ops::discard(&pool, failed).await.unwrap());
        assert_eq!(job_state(&pool, failed).await, "cancelled");
    })
    .await;
}

#[tokio::test]
async fn retry_failed_filters_by_kind_and_age_and_prune_preview_counts() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, state, max_attempts, max_crashes, deadline_s, lease_s, finished_at)
             VALUES ('a.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now() - interval '10 minutes'),
                    ('a.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now() - interval '3 days'),
                    ('b.v1', 'q', '{}', 'failed', 3, 2, 60, 60, now()),
                    ('b.v1', 'q', '{}', 'succeeded', 3, 2, 60, 60, now() - interval '2 days')",
        ).execute(&pool).await.unwrap();
        assert_eq!(ops::retry_failed(&pool, Some("a.v1"), Some(Duration::from_secs(3600))).await.unwrap(), 1);
        assert_eq!(ops::prune_preview(&pool, Duration::from_secs(86_400), Duration::from_secs(86_400)).await.unwrap(), (1, 1));
    }).await;
}

/// A failed job no longer holds its unique key, so a newer job may have taken it;
/// retrying the old one would violate `jobs_unique`.
#[tokio::test]
async fn retry_skips_jobs_whose_unique_key_is_taken() {
    with_db(|pool| async move {
        let old = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 1,
            },
        )
        .await;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now() WHERE id = $1")
            .bind(old)
            .execute(&pool)
            .await
            .unwrap();
        // Another failed row with the same key and a free key's failed row.
        let twin = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 2,
            },
        )
        .await;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now() WHERE id = $1")
            .bind(twin)
            .execute(&pool)
            .await
            .unwrap();
        let free = enqueue(
            &pool,
            &Unique {
                key: "free".into(),
                v: 3,
            },
        )
        .await;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now() WHERE id = $1")
            .bind(free)
            .execute(&pool)
            .await
            .unwrap();
        let holder = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 4,
            },
        )
        .await;

        assert!(
            !ops::retry(&pool, old).await.unwrap(),
            "key held by a waiting job"
        );
        assert_eq!(job_state(&pool, old).await, "failed");
        // Only the free key's row moves; the two "k" rows are skipped while `holder` waits.
        assert_eq!(ops::retry_failed(&pool, None, None).await.unwrap(), 1);
        assert_eq!(job_state(&pool, free).await, "available");
        assert_eq!(job_state(&pool, old).await, "failed");
        assert_eq!(job_state(&pool, twin).await, "failed");

        // Once the holder is gone, two failed rows with one key retry as one job, not two.
        assert!(ops::discard(&pool, holder).await.unwrap());
        assert_eq!(ops::retry_failed(&pool, None, None).await.unwrap(), 1);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM jobs WHERE unique_key = 'k' AND state = 'available'"
            )
            .await,
            1
        );
        // A running holder blocks too.
        let me = register_process(&pool, "me").await;
        let waiting_k = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM jobs WHERE unique_key = 'k' AND state = 'available'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        force_running(&pool, waiting_k, me).await;
        let still_failed = sqlx::query_scalar::<_, i64>(
            "SELECT id FROM jobs WHERE unique_key = 'k' AND state = 'failed' LIMIT 1",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(!ops::retry(&pool, still_failed).await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn retry_clears_a_stale_cancel_request() {
    with_db(|pool| async move {
        let id = enqueue(&pool, &Noop { n: 1 }).await;
        sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now(), cancel_requested = true WHERE id = $1")
            .bind(id).execute(&pool).await.unwrap();
        assert_eq!(ops::retry_failed(&pool, None, None).await.unwrap(), 1);
        let d = ops::show(&pool, id).await.unwrap().unwrap();
        assert!(!d.cancel_requested);
        assert_eq!(d.summary.state, "available");
        assert!(d.summary.finished_at.is_none());
    }).await;
}

async fn fail_job(pool: &sqlx::PgPool, id: i64) {
    sqlx::query("UPDATE jobs SET state = 'failed', finished_at = now() WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

/// Inserts a waiting job with this unique key inside an open transaction: the
/// retry's write then blocks on the unique index until the commit, and fails
/// with a unique violation (its `NOT EXISTS` check could not see the insert).
async fn hold_key_in_open_tx(
    pool: &sqlx::PgPool,
    key: &str,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s, unique_key)
         VALUES ('test_unique.v1', 'default', '{}', 3, 2, 60, 60, $1)",
    )
    .bind(key)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx
}

#[tokio::test]
async fn parallel_retries_of_twin_rows_move_exactly_one() {
    with_db(|pool| async move {
        let a = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 1,
            },
        )
        .await;
        fail_job(&pool, a).await;
        let b = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 2,
            },
        )
        .await;
        fail_job(&pool, b).await;
        let (ra, rb) = tokio::join!(ops::retry(&pool, a), ops::retry(&pool, b));
        let mut results = [ra.unwrap(), rb.unwrap()];
        results.sort();
        assert_eq!(results, [false, true]);
        assert_eq!(
            count(
                &pool,
                "SELECT count(*) FROM jobs WHERE unique_key = 'k' AND state = 'available'"
            )
            .await,
            1
        );
    })
    .await;
}

#[tokio::test]
async fn retry_returns_false_when_an_enqueue_commits_mid_write() {
    with_db(|pool| async move {
        let old = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 1,
            },
        )
        .await;
        fail_job(&pool, old).await;
        let tx = hold_key_in_open_tx(&pool, "k").await;
        let retrying = tokio::spawn({
            let pool = pool.clone();
            async move { ops::retry(&pool, old).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !retrying.is_finished(),
            "the write should be blocked on the open insert"
        );
        tx.commit().await.unwrap();
        assert!(!retrying.await.unwrap().unwrap());
        assert_eq!(job_state(&pool, old).await, "failed");
    })
    .await;
}

#[tokio::test]
async fn retry_failed_survives_a_key_taken_mid_statement() {
    with_db(|pool| async move {
        let conflicted = enqueue(
            &pool,
            &Unique {
                key: "k".into(),
                v: 1,
            },
        )
        .await;
        fail_job(&pool, conflicted).await;
        let free = enqueue(
            &pool,
            &Unique {
                key: "free".into(),
                v: 2,
            },
        )
        .await;
        fail_job(&pool, free).await;
        let tx = hold_key_in_open_tx(&pool, "k").await;
        let retrying = tokio::spawn({
            let pool = pool.clone();
            async move { ops::retry_failed(&pool, None, None).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            !retrying.is_finished(),
            "the batch should be blocked on the open insert"
        );
        tx.commit().await.unwrap();
        assert_eq!(
            retrying.await.unwrap().unwrap(),
            1,
            "the free row still moves"
        );
        assert_eq!(job_state(&pool, free).await, "available");
        assert_eq!(job_state(&pool, conflicted).await, "failed");
    })
    .await;
}

#[tokio::test]
async fn retry_failed_keeps_unique_keys_and_job_ids_apart() {
    with_db(|pool| async move {
        // Same kind: a keyless row whose id equals another row's unique_key text must both move.
        let keyless = enqueue(&pool, &Noop { n: 1 }).await;
        fail_job(&pool, keyless).await;
        let keyed: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (kind, queue, args, state, max_attempts, max_crashes, deadline_s, lease_s, unique_key, finished_at)
             VALUES ('test_noop.v1', 'default', '{}', 'failed', 3, 2, 60, 60, $1, now()) RETURNING id",
        )
        .bind(keyless.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(ops::retry_failed(&pool, None, None).await.unwrap(), 2);
        assert_eq!(job_state(&pool, keyless).await, "available");
        assert_eq!(job_state(&pool, keyed).await, "available");
    }).await;
}

#[tokio::test]
async fn the_default_filter_lists_the_newest_fifty() {
    with_db(|pool| async move {
        assert_eq!(ListFilter::default().limit, 50);
        for _ in 0..3 {
            common::new_job(&pool, "k").await;
        }
        let listed = ops::list(&pool, &ListFilter::default()).await.unwrap();
        assert_eq!(listed.len(), 3, "a default filter is not an empty page");
    })
    .await;
}

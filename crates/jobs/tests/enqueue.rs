#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::{Big, Noop, Unique, count, with_db};
use library_jobs::{EnqueueError, EnqueueOptions, Enqueued, Enqueuer};
use sqlx::postgres::PgListener;

#[tokio::test]
async fn commit_makes_the_job_visible_and_rollback_discards_it() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(true);
        let mut tx = pool.begin().await.unwrap();
        enqueuer.enqueue(&mut tx, &Noop { n: 1 }).await.unwrap();
        tx.rollback().await.unwrap();
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);

        let mut tx = pool.begin().await.unwrap();
        let id = enqueuer.enqueue(&mut tx, &Noop { n: 2 }).await.unwrap().id();
        tx.commit().await.unwrap();
        let (kind, queue, state, args, attempt, max_attempts, deadline_s, lease_s): (
            String, String, String, serde_json::Value, i32, i32, i32, i32,
        ) = sqlx::query_as(
            "SELECT kind, queue, state, args, attempt, max_attempts, deadline_s, lease_s FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((kind.as_str(), queue.as_str(), state.as_str()), ("test_noop.v1", "default", "available"));
        assert_eq!(args, serde_json::json!({ "n": 2 }));
        assert_eq!((attempt, max_attempts, deadline_s, lease_s), (0, 10, 900, 60));
    })
    .await;
}

#[tokio::test]
async fn notify_arrives_only_after_commit() {
    with_db(|pool| async move {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(library_jobs::NOTIFY_CHANNEL).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        Enqueuer::new(true)
            .enqueue(&mut tx, &Noop { n: 1 })
            .await
            .unwrap();
        let early = tokio::time::timeout(Duration::from_millis(300), listener.recv()).await;
        assert!(early.is_err(), "notification delivered before commit");
        tx.commit().await.unwrap();
        let note = tokio::time::timeout(Duration::from_secs(2), listener.recv())
            .await
            .expect("notification after commit")
            .unwrap();
        assert_eq!(note.payload(), "default");
    })
    .await;
}

#[tokio::test]
async fn no_notification_when_disabled() {
    with_db(|pool| async move {
        let mut listener = PgListener::connect_with(&pool).await.unwrap();
        listener.listen(library_jobs::NOTIFY_CHANNEL).await.unwrap();
        let mut conn = pool.acquire().await.unwrap();
        Enqueuer::new(false)
            .enqueue(&mut conn, &Noop { n: 1 })
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_millis(300), listener.recv()).await;
        assert!(got.is_err());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_unique_enqueues_create_one_job_and_never_fail() {
    with_db(|pool| async move {
        let mut tasks = Vec::new();
        for v in 0..16 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let mut tx = pool.begin().await.unwrap();
                let r = Enqueuer::new(true)
                    .enqueue(&mut tx, &Unique { key: "k".into(), v })
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                r
            }));
        }
        let mut new = Vec::new();
        let mut dup = Vec::new();
        for t in tasks {
            match t.await.unwrap() {
                Enqueued::New(id) => new.push(id),
                Enqueued::Duplicate(id) => dup.push(id),
            }
        }
        assert_eq!(new.len(), 1);
        assert_eq!(dup.len(), 15);
        assert!(dup.iter().all(|id| *id == new[0]));
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 1);
    })
    .await;
}

#[tokio::test]
async fn duplicate_while_running_then_new_after_finish() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(false);
        let mut conn = pool.acquire().await.unwrap();
        let first = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 1 }).await.unwrap().id();
        sqlx::query("UPDATE jobs SET state = 'running', locked_by = 1, locked_until = now() + interval '1 hour' WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let again = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 2 }).await.unwrap();
        assert_eq!(again, Enqueued::Duplicate(first));
        sqlx::query("UPDATE jobs SET state = 'succeeded', locked_by = NULL, locked_until = NULL, finished_at = now() WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        let later = enqueuer.enqueue(&mut conn, &Unique { key: "r".into(), v: 3 }).await.unwrap();
        assert!(matches!(later, Enqueued::New(id) if id != first));
    })
    .await;
}

#[tokio::test]
async fn replace_updates_a_waiting_duplicate_but_not_a_running_one() {
    with_db(|pool| async move {
        let enqueuer = Enqueuer::new(false);
        let mut conn = pool.acquire().await.unwrap();
        let replace = EnqueueOptions { replace: true, ..Default::default() };
        let id = enqueuer.enqueue(&mut conn, &Unique { key: "d".into(), v: 1 }).await.unwrap().id();
        let r = enqueuer.enqueue_with(&mut conn, &Unique { key: "d".into(), v: 2 }, replace.clone()).await.unwrap();
        assert_eq!(r, Enqueued::Duplicate(id));
        let v: i64 = sqlx::query_scalar("SELECT (args->>'v')::bigint FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(v, 2);
        sqlx::query("UPDATE jobs SET state = 'running', locked_by = 1, locked_until = now() + interval '1 hour' WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        enqueuer.enqueue_with(&mut conn, &Unique { key: "d".into(), v: 3 }, replace).await.unwrap();
        let v: i64 = sqlx::query_scalar("SELECT (args->>'v')::bigint FROM jobs WHERE id = $1").bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(v, 2, "a running job's arguments must not change");
    })
    .await;
}

#[tokio::test]
async fn oversized_arguments_are_rejected_before_any_write() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let err = Enqueuer::new(false)
            .enqueue(
                &mut conn,
                &Big {
                    s: "x".repeat(70 * 1024),
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(err, EnqueueError::TooLarge { .. }), "{err}");
        assert_eq!(count(&pool, "SELECT count(*) FROM jobs").await, 0);
    })
    .await;
}

#[tokio::test]
async fn delay_and_priority_are_stored() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let opts = EnqueueOptions {
            delay: Some(Duration::from_secs(60)),
            priority: Some(5),
            replace: false,
        };
        let id = Enqueuer::new(false)
            .enqueue_with(&mut conn, &Noop { n: 1 }, opts)
            .await
            .unwrap()
            .id();
        let (in_future, priority): (bool, i16) = sqlx::query_as(
            "SELECT run_at > now() + interval '59 seconds', priority FROM jobs WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(in_future);
        assert_eq!(priority, 5);
    })
    .await;
}

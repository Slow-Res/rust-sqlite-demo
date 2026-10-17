#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Explodes, Fails, Noop, ShortDeadline, Sleepy, register_process, registry, with_db};
use library_jobs::executor::{CancelCell, CancelReason, Execution, Finished, execute};
use library_jobs::store::claim::{ClaimRequest, claim};
use library_jobs::{Enqueuer, Job, RateLimiter};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

/// Enqueue `job`, claim it as a fresh process, and run it through `execute`.
async fn run<J: Job>(
    pool: &PgPool,
    job: J,
    before: impl FnOnce(&CancellationToken, &CancelCell),
) -> (i64, Finished) {
    let mut conn = pool.acquire().await.unwrap();
    let id = Enqueuer::new(false)
        .enqueue(&mut conn, &job)
        .await
        .unwrap()
        .id();
    run_claimed(pool, id, before).await
}

async fn run_claimed(
    pool: &PgPool,
    id: i64,
    before: impl FnOnce(&CancellationToken, &CancelCell),
) -> (i64, Finished) {
    let me = register_process(pool, &format!("p{id}")).await;
    let kinds = registry().kinds();
    let req = ClaimRequest {
        queue: "default",
        kinds: &kinds,
        process_id: me,
        limit: 1,
        max_running: None,
    };
    let job = claim(pool, &req).await.unwrap().pop().expect("claimed");
    assert_eq!(job.id, id);
    let token = CancellationToken::new();
    let cancel = Arc::new(CancelCell::default());
    before(&token, &cancel);
    let exec = Execution {
        job,
        process_id: me,
        token,
        cancel,
        progress: Arc::new(Mutex::new(None)),
    };
    (
        id,
        execute(pool, &registry(), Arc::new(RateLimiter::new()), exec).await,
    )
}

async fn row(pool: &PgPool, id: i64) -> (String, i32, Option<String>, Option<String>) {
    sqlx::query_as("SELECT state, attempt, errors->-1->>'cause', errors->-1->>'message' FROM jobs WHERE id = $1")
        .bind(id).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn success_is_recorded() {
    with_db(|pool| async move {
        let (id, f) = run(&pool, Noop { n: 7 }, |_, _| {}).await;
        assert_eq!(f, Finished::Succeeded);
        assert_eq!(row(&pool, id).await.0, "succeeded");
    })
    .await;
}

#[tokio::test]
async fn each_error_kind_maps_to_its_state() {
    with_db(|pool| async move {
        for (mode, finished, state, cause) in [
            ("retry", Finished::Retrying, "available", "error"),
            ("retry_after", Finished::Retrying, "available", "error"),
            ("permanent", Finished::Failed, "failed", "permanent"),
            ("panic", Finished::Retrying, "available", "panic"),
            ("cancelled", Finished::Cancelled, "cancelled", "cancelled"),
        ] {
            let (id, f) = run(&pool, Fails { mode: mode.into() }, |_, _| {}).await;
            assert_eq!(f, finished, "{mode}");
            let (st, _, c, _) = row(&pool, id).await;
            assert_eq!((st.as_str(), c.as_deref()), (state, Some(cause)), "{mode}");
        }
        let later: bool = sqlx::query_scalar(
            "SELECT run_at > now() + interval '119 s' FROM jobs WHERE args->>'mode' = 'retry_after'",
        ).fetch_one(&pool).await.unwrap();
        assert!(later, "Retry-After must be honoured");
    }).await;
}

#[tokio::test]
async fn a_panic_while_building_the_future_is_recorded_like_a_handler_panic() {
    with_db(|pool| async move {
        let (id, f) = run(&pool, Explodes { n: 1 }, |_, _| {}).await;
        assert_eq!(f, Finished::Failed, "max_attempts is 1");
        let (state, _, cause, msg) = row(&pool, id).await;
        assert_eq!(
            (state.as_str(), cause.as_deref()),
            ("failed", Some("panic"))
        );
        assert!(msg.unwrap().contains("while building its future"));
    })
    .await;
}

#[tokio::test]
async fn huge_error_message_is_truncated() {
    with_db(|pool| async move {
        let (id, _) = run(
            &pool,
            Fails {
                mode: "huge".into(),
            },
            |_, _| {},
        )
        .await;
        let (_, _, _, msg) = row(&pool, id).await;
        assert_eq!(msg.unwrap().chars().count(), 2000);
    })
    .await;
}

#[tokio::test]
async fn undecodable_args_fail_permanently() {
    with_db(|pool| async move {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             VALUES ('test_noop.v1', 'default', '{\"wrong\": true}', 10, 2, 900, 60) RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let (_, f) = run_claimed(&pool, id, |_, _| {}).await;
        assert_eq!(f, Finished::Failed);
        let (st, _, cause, msg) = row(&pool, id).await;
        assert_eq!(
            (st.as_str(), cause.as_deref()),
            ("failed", Some("bad_args"))
        );
        assert!(msg.unwrap().contains("missing field `n`"));
    })
    .await;
}

#[tokio::test]
async fn deadline_ends_the_attempt_and_retries() {
    with_db(|pool| async move {
        let start = tokio::time::Instant::now();
        let (id, f) = run(&pool, ShortDeadline { ms: 5_000 }, |_, _| {}).await;
        assert!(
            start.elapsed() < Duration::from_secs(3),
            "{:?}",
            start.elapsed()
        );
        assert_eq!(f, Finished::Retrying);
        assert_eq!(row(&pool, id).await.2.as_deref(), Some("deadline_exceeded"));
    })
    .await;
}

#[tokio::test]
async fn cancel_reasons_decide_the_write() {
    with_db(|pool| async move {
        let (id, f) = run(
            &pool,
            Sleepy {
                ms: 10_000,
                honour_cancel: true,
            },
            |t, c| {
                c.set(CancelReason::Requested);
                t.cancel();
            },
        )
        .await;
        assert_eq!(
            (f, row(&pool, id).await.0.as_str()),
            (Finished::Cancelled, "cancelled")
        );

        let (id, f) = run(
            &pool,
            Sleepy {
                ms: 10_000,
                honour_cancel: true,
            },
            |t, c| {
                c.set(CancelReason::Shutdown);
                t.cancel();
            },
        )
        .await;
        assert_eq!(f, Finished::Released);
        let (st, attempt, cause, _) = row(&pool, id).await;
        assert_eq!(
            (st.as_str(), attempt, cause.as_deref()),
            ("available", 0, Some("released_on_shutdown"))
        );
        // A released job is claimable again (that is the point); park it so the next claim gets the next job.
        sqlx::query("UPDATE jobs SET state = 'cancelled', finished_at = now() WHERE id = $1")
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();

        let (id, f) = run(
            &pool,
            Sleepy {
                ms: 10_000,
                honour_cancel: true,
            },
            |t, c| {
                c.set(CancelReason::LeaseLost);
                t.cancel();
            },
        )
        .await;
        assert_eq!(f, Finished::LeaseLost);
        assert_eq!(
            row(&pool, id).await.0,
            "running",
            "a lost lease writes nothing; the reaper owns the job now"
        );
    })
    .await;
}

#[tokio::test]
async fn a_stolen_job_is_not_overwritten() {
    with_db(|pool| async move {
        let mut conn = pool.acquire().await.unwrap();
        let id = Enqueuer::new(false)
            .enqueue(
                &mut conn,
                &Sleepy {
                    ms: 300,
                    honour_cancel: false,
                },
            )
            .await
            .unwrap()
            .id();
        let pool2 = pool.clone();
        let thief = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            sqlx::query("UPDATE jobs SET locked_by = -42 WHERE id = $1")
                .bind(id)
                .execute(&pool2)
                .await
                .unwrap();
        });
        let (_, f) = run_claimed(&pool, id, |_, _| {}).await;
        thief.await.unwrap();
        assert_eq!(f, Finished::FenceRejected);
        assert_eq!(row(&pool, id).await.0, "running");
    })
    .await;
}

#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::{force_running, new_job, register_process, with_db};
use library_jobs::store::finish::{
    Cause, Fence, MAX_ERROR_CHARS, Next, cancel, fail_attempt, release, succeed, write_progress,
};
use sqlx::PgPool;

async fn state(pool: &PgPool, id: i64) -> (String, i32, bool, bool) {
    sqlx::query_as(
        "SELECT state, attempt, locked_by IS NULL, finished_at IS NOT NULL FROM jobs WHERE id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
async fn succeed_needs_the_exact_claim_identity() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        assert!(
            !succeed(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me + 1,
                    attempt
                }
            )
            .await
            .unwrap()
        );
        assert!(
            !succeed(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me,
                    attempt: attempt + 1
                }
            )
            .await
            .unwrap()
        );
        assert_eq!(state(&pool, id).await.0, "running");
        assert!(
            succeed(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me,
                    attempt
                }
            )
            .await
            .unwrap()
        );
        assert_eq!(state(&pool, id).await, ("succeeded".into(), 1, true, true));
        assert!(
            !succeed(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me,
                    attempt
                }
            )
            .await
            .unwrap(),
            "only once"
        );
    })
    .await;
}

#[tokio::test]
async fn failed_attempts_retry_later_or_fail_and_record_the_cause() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        let f = Fence { job_id: id, process_id: me, attempt };
        assert!(fail_attempt(&pool, f, Cause::Error, "boom", Next::RetryIn(Duration::from_secs(30))).await.unwrap());
        let (st, later, cause, msg): (String, bool, String, String) = sqlx::query_as(
            "SELECT state, run_at > now() + interval '29 s', errors->0->>'cause', errors->0->>'message' FROM jobs WHERE id = $1",
        ).bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!((st.as_str(), later, cause.as_str(), msg.as_str()), ("available", true, "error", "boom"));

        let attempt = force_running(&pool, id, me).await;
        let f = Fence { job_id: id, process_id: me, attempt };
        assert!(fail_attempt(&pool, f, Cause::Permanent, "bad input", Next::Fail).await.unwrap());
        assert_eq!(state(&pool, id).await, ("failed".into(), 2, true, true));
    })
    .await;
}

#[tokio::test]
async fn errors_are_capped_and_truncated() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let huge = "é".repeat(1_000_000);
        for _ in 0..12 {
            let attempt = force_running(&pool, id, me).await;
            fail_attempt(&pool, Fence { job_id: id, process_id: me, attempt }, Cause::Error, &huge, Next::RetryIn(Duration::ZERO))
                .await.unwrap();
        }
        let (n, first_attempt, len): (i32, i32, i32) = sqlx::query_as(
            "SELECT jsonb_array_length(errors), (errors->0->>'attempt')::int, length(errors->0->>'message') FROM jobs WHERE id = $1",
        ).bind(id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 10);
        assert_eq!(first_attempt, 3, "the two oldest entries were dropped");
        assert_eq!(len as usize, MAX_ERROR_CHARS);
    })
    .await;
}

#[tokio::test]
async fn release_gives_the_attempt_back_and_cancel_is_terminal() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let a = new_job(&pool, "k").await;
        let attempt = force_running(&pool, a, me).await;
        assert!(
            release(
                &pool,
                Fence {
                    job_id: a,
                    process_id: me,
                    attempt
                }
            )
            .await
            .unwrap()
        );
        assert_eq!(state(&pool, a).await, ("available".into(), 0, true, false));
        let b = new_job(&pool, "k").await;
        let attempt = force_running(&pool, b, me).await;
        assert!(
            cancel(
                &pool,
                Fence {
                    job_id: b,
                    process_id: me,
                    attempt
                },
                "operator"
            )
            .await
            .unwrap()
        );
        assert_eq!(state(&pool, b).await, ("cancelled".into(), 1, true, true));
    })
    .await;
}

#[tokio::test]
async fn progress_writes_are_fenced() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let id = new_job(&pool, "k").await;
        let attempt = force_running(&pool, id, me).await;
        let p = serde_json::json!({ "done": 3, "total": 10 });
        assert!(
            write_progress(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me,
                    attempt
                },
                &p
            )
            .await
            .unwrap()
        );
        assert!(
            !write_progress(
                &pool,
                Fence {
                    job_id: id,
                    process_id: me + 1,
                    attempt
                },
                &serde_json::json!({})
            )
            .await
            .unwrap()
        );
        let stored: serde_json::Value =
            sqlx::query_scalar("SELECT progress FROM jobs WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(stored, p);
    })
    .await;
}

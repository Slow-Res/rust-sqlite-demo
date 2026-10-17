#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::{count, register_process, with_db};
use library_jobs::store::claim::{ClaimRequest, claim};
use sqlx::PgPool;

async fn add(
    pool: &PgPool,
    queue: &str,
    kind: &str,
    priority: i16,
    delay_s: f64,
    key: Option<&str>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO jobs (kind, queue, args, priority, run_at, max_attempts, max_crashes, deadline_s, lease_s, concurrency_key)
         VALUES ($1, $2, '{}', $3, now() + make_interval(secs => $4::float8), 10, 2, 900, 60, $5) RETURNING id",
    )
    .bind(kind).bind(queue).bind(priority).bind(delay_s).bind(key)
    .fetch_one(pool).await.unwrap()
}

fn kinds() -> Vec<String> {
    vec!["k".to_owned()]
}

#[tokio::test]
async fn claims_by_priority_then_age_up_to_the_limit_and_sets_the_lease() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let low = add(&pool, "q", "k", 5, -10.0, None).await;
        let old = add(&pool, "q", "k", 0, -20.0, None).await;
        let new = add(&pool, "q", "k", 0, -1.0, None).await;
        let kinds = kinds();
        let req = ClaimRequest { queue: "q", kinds: &kinds, process_id: me, limit: 2, max_running: None };
        let mut got: Vec<i64> = claim(&pool, &req).await.unwrap().into_iter().map(|j| j.id).collect();
        got.sort();
        let mut want = vec![old, new];
        want.sort();
        assert_eq!(got, want, "priority 0 before 5; limit 2");
        let (attempt, leased, started): (i32, bool, bool) = sqlx::query_as(
            "SELECT attempt, locked_until BETWEEN now() + interval '59 s' AND now() + interval '61 s',
                    attempt_started_at IS NOT NULL FROM jobs WHERE id = $1",
        ).bind(old).fetch_one(&pool).await.unwrap();
        assert_eq!((attempt, leased, started), (1, true, true));
        let rest = claim(&pool, &req).await.unwrap();
        assert_eq!(rest.iter().map(|j| j.id).collect::<Vec<_>>(), vec![low]);
    })
    .await;
}

#[tokio::test]
async fn skips_unknown_kinds_future_jobs_and_paused_queues() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        add(&pool, "q", "other", 0, -1.0, None).await;
        add(&pool, "q", "k", 0, 60.0, None).await;
        add(&pool, "p", "k", 0, -1.0, None).await;
        sqlx::query("INSERT INTO job_queue_pauses (queue, paused_by) VALUES ('p', 'test')")
            .execute(&pool)
            .await
            .unwrap();
        let kinds = kinds();
        for queue in ["q", "p"] {
            let req = ClaimRequest {
                queue,
                kinds: &kinds,
                process_id: me,
                limit: 10,
                max_running: None,
            };
            assert!(
                claim(&pool, &req).await.unwrap().is_empty(),
                "queue {queue}"
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_claimers_never_take_the_same_job() {
    with_db(|pool| async move {
        sqlx::query(
            "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
             SELECT 'k', 'q', '{}', 10, 2, 900, 60 FROM generate_series(1, 2000)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let mut tasks = Vec::new();
        for i in 0..8 {
            let pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let me = register_process(&pool, &format!("p{i}")).await;
                let kinds = kinds();
                let mut mine = Vec::new();
                loop {
                    let req = ClaimRequest {
                        queue: "q",
                        kinds: &kinds,
                        process_id: me,
                        limit: 25,
                        max_running: None,
                    };
                    let got = claim(&pool, &req).await.unwrap();
                    if got.is_empty() {
                        break;
                    }
                    mine.extend(got.into_iter().map(|j| j.id));
                }
                mine
            }));
        }
        let mut all = Vec::new();
        for t in tasks {
            all.extend(t.await.unwrap());
        }
        let unique: HashSet<i64> = all.iter().copied().collect();
        assert_eq!(all.len(), 2000);
        assert_eq!(unique.len(), 2000, "a job was claimed twice");
    })
    .await;
}

/// Claim in a loop from `claimers` tasks, finishing each job after `hold`,
/// while a sampler checks `bad_sql` (a count that must stay 0) every 5 ms.
async fn stress(
    pool: PgPool,
    queue: &'static str,
    max_running: Option<i64>,
    bad_sql: &'static str,
) {
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (pool, stop) = (pool.clone(), stop.clone());
        tokio::spawn(async move {
            let mut worst = 0;
            while !stop.load(Ordering::Relaxed) {
                worst = worst.max(count(&pool, bad_sql).await);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            worst
        })
    };
    let mut tasks = Vec::new();
    for i in 0..6 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            let me = register_process(&pool, &format!("{queue}{i}")).await;
            let kinds = kinds();
            let mut idle = 0;
            while idle < 20 {
                let req = ClaimRequest { queue, kinds: &kinds, process_id: me, limit: 5, max_running };
                let got = claim(&pool, &req).await.unwrap();
                if got.is_empty() { idle += 1; tokio::time::sleep(Duration::from_millis(10)).await; continue; }
                idle = 0;
                tokio::time::sleep(Duration::from_millis(20)).await;
                for j in got {
                    sqlx::query("UPDATE jobs SET state = 'succeeded', finished_at = now(), locked_by = NULL, locked_until = NULL WHERE id = $1")
                        .bind(j.id).execute(&pool).await.unwrap();
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    assert_eq!(sampler.await.unwrap(), 0, "limit exceeded: {bad_sql}");
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM jobs WHERE state <> 'succeeded'"
        )
        .await,
        0,
        "jobs left over"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn queue_max_running_is_never_exceeded() {
    with_db(|pool| async move {
        for _ in 0..40 { add(&pool, "imports", "k", 0, -1.0, None).await; }
        stress(pool, "imports", Some(2),
            "SELECT greatest(count(*) - 2, 0) FROM jobs WHERE queue = 'imports' AND state = 'running'").await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn at_most_one_running_job_per_concurrency_key() {
    with_db(|pool| async move {
        for i in 0..30 {
            add(
                &pool,
                "keyed",
                "k",
                0,
                -1.0,
                Some(if i % 2 == 0 { "a" } else { "b" }),
            )
            .await;
        }
        stress(
            pool,
            "keyed",
            None,
            "SELECT count(*) FROM (SELECT concurrency_key FROM jobs WHERE state = 'running'
                                   GROUP BY concurrency_key HAVING count(*) > 1) x",
        )
        .await;
    })
    .await;
}

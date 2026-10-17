#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{insert_running, register_process, with_db};
use library_jobs::store::process::{Beat, deregister, heartbeat, set_stopping};

#[tokio::test]
async fn heartbeat_renews_only_this_process_jobs_within_their_deadline() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let other = register_process(&pool, "other").await;
        let mine = insert_running(&pool, me, "k", 60, 900).await;
        let theirs = insert_running(&pool, other, "k", 60, 900).await;
        let overdue = insert_running(&pool, me, "k", 60, 60).await;
        sqlx::query("UPDATE jobs SET attempt_started_at = now() - interval '2 minutes', locked_until = now() + interval '1 second' WHERE id = ANY($1)")
            .bind(vec![mine, theirs, overdue])
            .execute(&pool)
            .await
            .unwrap();
        // `mine` is past 2 min of a 900 s deadline: fine. `overdue` is past its 60 s deadline.
        let beat = heartbeat(&pool, me, &[mine, theirs, overdue]).await.unwrap();
        let Beat::Alive(rows) = beat else { panic!("pruned") };
        let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec![mine]);
        let renewed: bool = sqlx::query_scalar("SELECT locked_until > now() + interval '59 seconds' FROM jobs WHERE id = $1")
            .bind(mine).fetch_one(&pool).await.unwrap();
        assert!(renewed);
        let untouched: bool = sqlx::query_scalar("SELECT locked_until < now() + interval '2 seconds' FROM jobs WHERE id = $1")
            .bind(theirs).fetch_one(&pool).await.unwrap();
        assert!(untouched, "another process's lease must not move");
    })
    .await;
}

#[tokio::test]
async fn heartbeat_reports_cancel_requests_and_updates_the_registry() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        let job = insert_running(&pool, me, "k", 60, 900).await;
        sqlx::query("UPDATE job_processes SET last_heartbeat_at = now() - interval '1 minute'").execute(&pool).await.unwrap();
        sqlx::query("UPDATE jobs SET cancel_requested = true WHERE id = $1").bind(job).execute(&pool).await.unwrap();
        let Beat::Alive(rows) = heartbeat(&pool, me, &[job]).await.unwrap() else { panic!("pruned") };
        assert!(rows[0].cancel_requested);
        let fresh: bool = sqlx::query_scalar("SELECT last_heartbeat_at > now() - interval '1 second' FROM job_processes WHERE id = $1")
            .bind(me).fetch_one(&pool).await.unwrap();
        assert!(fresh);
    })
    .await;
}

#[tokio::test]
async fn a_deleted_registry_row_reads_as_pruned() {
    with_db(|pool| async move {
        let me = register_process(&pool, "me").await;
        set_stopping(&pool, me).await.unwrap();
        deregister(&pool, me).await.unwrap();
        assert!(matches!(
            heartbeat(&pool, me, &[]).await.unwrap(),
            Beat::Pruned
        ));
    })
    .await;
}

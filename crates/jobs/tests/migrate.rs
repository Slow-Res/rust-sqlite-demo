#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use common::{count, with_db};

#[tokio::test]
async fn migrate_creates_schema_in_its_own_history_table_and_is_idempotent() {
    with_db(|pool| async move {
        // TestDb already migrated once; a second run must be a no-op.
        library_jobs::migrate(&pool).await.unwrap();
        for table in [
            "jobs",
            "job_processes",
            "job_queue_pauses",
            "job_recurring_runs",
        ] {
            let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
                .bind(table)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert!(exists, "{table} missing");
        }
        assert_eq!(
            count(&pool, "SELECT count(*) FROM _jobs_migrations").await,
            1
        );
        let app_table: bool =
            sqlx::query_scalar("SELECT to_regclass('_sqlx_migrations') IS NOT NULL")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(
            !app_table,
            "the jobs migrator must not touch the app's history table"
        );
    })
    .await;
}

#[tokio::test]
async fn push_error_keeps_the_last_ten() {
    with_db(|pool| async move {
        let errs: serde_json::Value = sqlx::query_scalar(
            "SELECT jobs_push_error(
                 (SELECT jsonb_agg(jsonb_build_object('n', i) ORDER BY i) FROM generate_series(1, 10) i),
                 jsonb_build_object('n', 11))",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let ns: Vec<i64> = errs.as_array().unwrap().iter().map(|e| e["n"].as_i64().unwrap()).collect();
        assert_eq!(ns, (2..=11).collect::<Vec<i64>>(), "oldest dropped, order kept");
    })
    .await;
}

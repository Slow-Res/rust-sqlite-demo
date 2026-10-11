//! Migrations during a rollback or rolling deploy (spec S10).
#![allow(clippy::unwrap_used)]

use sqlx::SqlitePool;

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn a_newer_applied_migration_does_not_stop_the_older_binary(pool: SqlitePool) {
    // What the previous release sees after a newer one ran an expand migration.
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES (99991231000000, 'from a newer release', 1, x'00', 0)",
    )
    .execute(&pool)
    .await
    .unwrap();
    library_web::db::migrate(&pool).await.unwrap();
}

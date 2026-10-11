//! The schema handshake (spec §6.7).
#![allow(clippy::unwrap_used)]

mod common;

use common::TestSqlite;
use sync_worker::schema::sqlite_schema_ready;

#[tokio::test]
async fn ready_only_when_every_known_migration_is_applied() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("empty.db").display());
    let empty = sqlite_setup::open_pool(&url, 1, true).await.unwrap();
    assert!(
        !sqlite_schema_ready(&empty).await.unwrap(),
        "no _sqlx_migrations table yet"
    );

    let db = TestSqlite::new().await;
    assert!(sqlite_schema_ready(&db.pool).await.unwrap());
    sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    assert!(
        !sqlite_schema_ready(&db.pool).await.unwrap(),
        "newest migration missing"
    );
}

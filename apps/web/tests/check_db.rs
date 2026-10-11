//! `library-web check-db` (spec §5 S15, §7.3).
#![allow(clippy::unwrap_used)]

mod common;

use library_web::commands::{self, inspect, render};
use library_web::config::Config;
use sqlx::SqlitePool;

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn a_seeded_database_is_ok_and_counted(pool: SqlitePool) {
    library_web::seed::seed(&pool, chrono::Utc::now())
        .await
        .unwrap();
    let inspection = inspect(&pool).await.unwrap();
    assert!(inspection.report.is_ok());
    assert_eq!(
        inspection.counts,
        vec![("books", 12), ("members", 6), ("loans", 3), ("users", 0)]
    );
    let text = render("sqlite://x.db", &inspection);
    assert_eq!(
        text,
        "database: sqlite://x.db\nintegrity: ok\nforeign keys: ok\nbooks: 12\nmembers: 6\nloans: 3\nusers: 0\n"
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn broken_references_are_reported(pool: SqlitePool) {
    let mut conn = pool.acquire().await.unwrap();
    sqlx::query("PRAGMA foreign_keys = OFF")
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES (999, 999, 0, 1)",
    )
    .execute(&mut *conn)
    .await
    .unwrap();
    sqlx::query("PRAGMA foreign_keys = ON")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let inspection = inspect(&pool).await.unwrap();
    assert!(!inspection.report.is_ok());
    assert!(
        render("x", &inspection).contains("foreign keys: loans row 1 points to a missing row in")
    );
}

#[tokio::test]
async fn a_missing_file_is_an_error_and_is_not_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("restored.db");
    let config = Config {
        database_url: format!("sqlite://{}", path.display()),
        ..common::test_config()
    };
    assert!(commands::check_db(config).await.is_err());
    assert!(!path.exists(), "check-db must never create a database");
}

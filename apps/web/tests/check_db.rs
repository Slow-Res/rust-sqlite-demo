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

#[tokio::test]
async fn a_corrupted_file_fails_the_check() {
    use std::io::{Seek, SeekFrom, Write};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corrupt.db");
    let config = Config {
        database_url: format!("sqlite://{}", path.display()),
        ..common::test_config()
    };
    let pool = library_web::db::connect(&config).await.unwrap();
    library_web::db::migrate(&pool).await.unwrap();
    for i in 0..300 {
        sqlx::query(
            "INSERT INTO books (title, author, total_copies, created_at, updated_at) \
             VALUES (?, 'Author', 1, 0, 0)",
        )
        .bind(format!("book {i} {}", "x".repeat(100)))
        .execute(&pool)
        .await
        .unwrap();
    }
    // Closing the last connection folds the WAL into the main file.
    pool.close().await;
    let len = std::fs::metadata(&path).unwrap().len();
    assert!(
        len >= 8 * 4096,
        "the file needs several pages, has {len} bytes"
    );

    // Overwrite a whole page in the middle, leaving the header and schema intact.
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.seek(SeekFrom::Start(3 * 4096)).unwrap();
    file.write_all(&[0xAB; 4096]).unwrap();
    drop(file);

    // The check itself reports a problem...
    let pool = sqlite_setup::open_pool(&config.database_url, 1, false)
        .await
        .unwrap();
    let report = sqlite_setup::check(&pool).await;
    assert!(
        report.map_or(true, |r| !r.is_ok()),
        "integrity_check must not pass on a corrupted page"
    );
    pool.close().await;
    // ...and the command exits with an error.
    assert!(commands::check_db(config).await.is_err());
}

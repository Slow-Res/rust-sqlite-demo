//! The background WAL checkpoint (`SQLITE_CHECKPOINT_INTERVAL_MS`): with readers
//! active, the WAL file shrinks after it runs. SQLite-only, on a real file (an
//! in-memory database has no WAL).
#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use library_web::checkpoint;
use library_web::config::Config;
use library_web::db;
use sqlx::SqlitePool;

async fn open(dir: &Path) -> SqlitePool {
    let config = Config {
        database_url: format!("sqlite://{}", dir.join("library.db").display()),
        ..common::test_config()
    };
    let database = db::connect(&config).await.unwrap();
    db::migrate(&database).await.unwrap();
    database.require_sqlite("test").unwrap().clone()
}

async fn insert_books(pool: &SqlitePool, count: usize) {
    for i in 0..count {
        sqlx::query(
            "INSERT INTO books (title, author, total_copies, created_at, updated_at) \
             VALUES (?, 'Author', 1, 0, 0)",
        )
        .bind(format!("book {i} {}", "x".repeat(200)))
        .execute(pool)
        .await
        .unwrap();
    }
}

fn wal_len(dir: &Path) -> u64 {
    std::fs::metadata(dir.join("library.db-wal"))
        .map(|m| m.len())
        .unwrap_or(0)
}

/// Readers that keep querying until the returned flag is set.
fn start_readers(pool: &SqlitePool) -> Arc<AtomicBool> {
    let stop = Arc::new(AtomicBool::new(false));
    for _ in 0..4 {
        let (pool, stop) = (pool.clone(), stop.clone());
        tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM books")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            }
        });
    }
    stop
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn truncate_empties_the_wal_while_readers_are_active() {
    let dir = tempfile::tempdir().unwrap();
    let pool = open(dir.path()).await;
    insert_books(&pool, 300).await;
    let before = wal_len(dir.path());
    assert!(before > 0, "the writes must have left a WAL");

    let stop = start_readers(&pool);
    let outcome = checkpoint::checkpoint_truncate(&pool).await.unwrap();
    stop.store(true, Ordering::Relaxed);

    assert!(!outcome.busy, "{outcome:?}");
    assert_eq!(wal_len(dir.path()), 0, "was {before} bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_background_task_keeps_the_wal_small() {
    let dir = tempfile::tempdir().unwrap();
    let pool = open(dir.path()).await;
    insert_books(&pool, 300).await;
    assert!(wal_len(dir.path()) > 0);

    let stop = start_readers(&pool);
    let task = checkpoint::spawn(pool.clone(), Duration::from_millis(50));
    let mut truncated = false;
    for _ in 0..100 {
        if wal_len(dir.path()) == 0 {
            truncated = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    task.abort();
    stop.store(true, Ordering::Relaxed);
    assert!(truncated, "the WAL was never truncated");
}

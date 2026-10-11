//! Helpers for the worker's integration tests: a real SQLite file in a temp
//! directory and a fresh Postgres database per test.
//!
//! Each file in `tests/` is its own crate and uses only some helpers, hence
//! the `dead_code` allowance; panicking on broken setup is the right
//! behaviour in tests.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used)]

use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sqlx::postgres::PgConnectOptions;
use sqlx::{AssertSqlSafe, Connection, PgConnection, SqlitePool};

/// A node's SQLite database as `library-web` leaves it: file created, migrated.
pub struct TestSqlite {
    /// Kept so the directory lives as long as the database.
    pub dir: tempfile::TempDir,
    pub url: String,
    pub pool: SqlitePool,
}

impl TestSqlite {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let url = format!("sqlite://{}", dir.path().join("library.db").display());
        let pool = sqlite_setup::open_pool(&url, 4, true)
            .await
            .expect("open SQLite");
        sync_worker::SQLITE_MIGRATIONS
            .run(&pool)
            .await
            .expect("migrate SQLite");
        Self { dir, url, pool }
    }
}

/// A fresh, empty database on the test server, one per test. The server comes
/// from TEST_POSTGRES_URL; `make test` starts one in Docker.
pub struct TestPg {
    admin: PgConnectOptions,
    name: String,
    /// Connect options for this test's database.
    pub options: PgConnectOptions,
}

impl TestPg {
    pub async fn new() -> Self {
        let url = std::env::var("TEST_POSTGRES_URL").expect(
            "TEST_POSTGRES_URL is not set. Run `make test` (it starts a throwaway Postgres in Docker), \
             or set it to any Postgres where you may create databases.",
        );
        let admin = PgConnectOptions::from_str(&url).expect("valid TEST_POSTGRES_URL");
        let name = unique_name();
        let mut conn = PgConnection::connect_with(&admin)
            .await
            .expect("connect to test Postgres");
        sqlx::query(AssertSqlSafe(format!("CREATE DATABASE {name}")))
            .execute(&mut conn)
            .await
            .expect("create test database");
        conn.close().await.ok();
        let options = admin.clone().database(&name);
        Self {
            admin,
            name,
            options,
        }
    }

    pub async fn connect(&self) -> PgConnection {
        PgConnection::connect_with(&self.options)
            .await
            .expect("connect to test database")
    }

    /// Drop this test's database. Tests call it last; a failed test leaves
    /// its database behind in the throwaway server, which is harmless.
    pub async fn cleanup(self) {
        let mut conn = PgConnection::connect_with(&self.admin)
            .await
            .expect("connect to test Postgres");
        sqlx::query(AssertSqlSafe(format!(
            "DROP DATABASE {} WITH (FORCE)",
            self.name
        )))
        .execute(&mut conn)
        .await
        .expect("drop test database");
        conn.close().await.ok();
    }
}

fn unique_name() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!(
        "sw_test_{}_{nanos}_{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Insert a book like the app would; returns its id.
pub async fn insert_book(pool: &SqlitePool, title: &str) -> i64 {
    sqlx::query(
        "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) \
         VALUES (?, 'Author', NULL, 2001, 1, 0, 0)",
    )
    .bind(title)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

/// Insert `count` books titled `bulk 1`, `bulk 2`... in one transaction.
pub async fn insert_books(pool: &SqlitePool, count: usize) {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for i in 1..=count {
        sqlx::query(
            "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) \
             VALUES (?, 'Author', NULL, 2001, 1, 0, 0)",
        )
        .bind(format!("bulk {i}"))
        .execute(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
}

pub async fn rename_book(pool: &SqlitePool, id: i64, title: &str) {
    sqlx::query("UPDATE books SET title = ? WHERE id = ?")
        .bind(title)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

pub async fn delete_book(pool: &SqlitePool, id: i64) {
    sqlx::query("DELETE FROM books WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
}

pub async fn insert_member(pool: &SqlitePool, email: &str) -> i64 {
    sqlx::query(
        "INSERT INTO members (name, email, created_at, updated_at) VALUES ('Member', ?, 0, 0)",
    )
    .bind(email)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

pub async fn insert_loan(pool: &SqlitePool, book_id: i64, member_id: i64) -> i64 {
    sqlx::query("INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES (?, ?, 0, 100, NULL)")
        .bind(book_id).bind(member_id).execute(pool).await.unwrap().last_insert_rowid()
}

/// Connect a Mirror, retrying for up to 2 s while the node's lock is still
/// held by a connection that is closing (Postgres releases it a moment after
/// the client hangs up).
pub async fn connect_mirror(pg: &TestPg, node_id: &str) -> sync_worker::mirror::Mirror {
    use sync_worker::mirror::{ConnectError, Mirror};
    for _ in 0..40 {
        match Mirror::connect(&pg.options, node_id).await {
            Ok(mirror) => return mirror,
            Err(ConnectError::Locked(_)) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await
            }
            Err(ConnectError::Other(err)) => panic!("connect failed: {err:#}"),
        }
    }
    panic!("the lock for {node_id} was never released");
}

impl TestSqlite {
    /// Copy the database as it is right now into a new file (`VACUUM INTO`),
    /// like a backup taken at this moment, and open the copy.
    pub async fn backup(&self) -> TestSqlite {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("library.db");
        sqlx::query("VACUUM INTO ?")
            .bind(path.display().to_string())
            .execute(&self.pool)
            .await
            .expect("VACUUM INTO");
        let url = format!("sqlite://{}", path.display());
        let pool = sqlite_setup::open_pool(&url, 4, false)
            .await
            .expect("open backup");
        TestSqlite { dir, url, pool }
    }
}

/// Every synced table holds exactly the same rows in SQLite and in Postgres.
pub async fn assert_in_sync(sqlite: &SqlitePool, pg: &TestPg, node_id: &str) {
    let mut conn = pg.connect().await;
    for table in sync_worker::tables::TABLES {
        let local = sync_worker::changes::read_all(sqlite, table).await.unwrap();
        let remote = sync_worker::mirror::read_all(&mut conn, node_id, table)
            .await
            .unwrap();
        assert_eq!(
            local, remote,
            "table `{}` differs between SQLite and Postgres",
            table.name
        );
    }
    conn.close().await.ok();
}

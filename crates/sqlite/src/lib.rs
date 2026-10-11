//! The one place SQLite is opened and configured (spec §5).
//!
//! Both programs that open a node's database file, `library-web` and
//! `sync-worker`, go through [`open_pool`], so they can never disagree on the
//! settings below. The settings live in the connect options, which the pool
//! applies to *every* connection it opens, not just the first (rule S8).

use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

/// How long a connection waits for the write lock before failing with
/// `SQLITE_BUSY` (rule S4).
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect options with the production settings applied:
///
/// - WAL journal: readers never block the writer, and Litestream needs it (S2).
/// - `synchronous = NORMAL`: safe with WAL. A power cut can lose the last
///   commits but never corrupts the file (S3).
/// - `busy_timeout`: wait for the write lock instead of failing at once (S4).
/// - `foreign_keys = ON`: SQLite leaves it off unless each connection asks (S5).
///
/// Write transactions should start with `pool.begin_with("BEGIN IMMEDIATE")`.
/// A plain `BEGIN` that reads and then writes can fail with `SQLITE_BUSY`
/// halfway when another connection took the write lock in between (S6).
pub fn connect_options(url: &str, create_if_missing: bool) -> anyhow::Result<SqliteConnectOptions> {
    let options = SqliteConnectOptions::from_str(url)
        .with_context(|| format!("invalid SQLite URL `{url}`"))?
        .create_if_missing(create_if_missing)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(BUSY_TIMEOUT);
    Ok(options)
}

/// Open a pool on `url`. With `create_if_missing`, also create the file and
/// its parent directory; without it, a missing file is an error.
pub async fn open_pool(
    url: &str,
    max_connections: u32,
    create_if_missing: bool,
) -> anyhow::Result<SqlitePool> {
    let options = connect_options(url, create_if_missing)?;
    if create_if_missing
        && let Some(dir) = options
            .get_filename()
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }
    SqlitePoolOptions::new()
        .max_connections(max_connections)
        .connect_with(options)
        .await
        .with_context(|| format!("opening SQLite database `{url}`"))
}

/// Result of [`check`]. Empty lists mean the database is healthy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// Lines from `PRAGMA integrity_check` other than its single `ok`.
    pub integrity_errors: Vec<String>,
    /// One line per row from `PRAGMA foreign_key_check`.
    pub foreign_key_errors: Vec<String>,
}

impl CheckReport {
    pub fn is_ok(&self) -> bool {
        self.integrity_errors.is_empty() && self.foreign_key_errors.is_empty()
    }
}

/// Look for corruption and broken references (rule S15). It reads the whole
/// file, so it takes a while on a big database.
pub async fn check(pool: &SqlitePool) -> anyhow::Result<CheckReport> {
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(pool)
        .await
        .context("running PRAGMA integrity_check")?;
    let integrity_errors = integrity.into_iter().filter(|line| line != "ok").collect();

    let rows = sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .context("running PRAGMA foreign_key_check")?;
    let foreign_key_errors = rows
        .iter()
        .map(|row| -> Result<String, sqlx::Error> {
            let table: String = row.try_get(0)?;
            let rowid: Option<i64> = row.try_get(1)?;
            let parent: String = row.try_get(2)?;
            let rowid = rowid.map_or_else(|| "?".to_owned(), |id| id.to_string());
            Ok(format!(
                "{table} row {rowid} points to a missing row in {parent}"
            ))
        })
        .collect::<Result<Vec<_>, _>>()
        .context("reading PRAGMA foreign_key_check")?;

    Ok(CheckReport {
        integrity_errors,
        foreign_key_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_url(dir: &tempfile::TempDir, name: &str) -> String {
        format!("sqlite://{}", dir.path().join(name).display())
    }

    #[tokio::test]
    async fn every_connection_gets_the_settings() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_pool(&temp_url(&dir, "t.db"), 2, true).await.unwrap();
        // Hold two connections at once so the pool has to open a second one.
        let mut first = pool.acquire().await.unwrap();
        let mut second = pool.acquire().await.unwrap();
        for conn in [&mut first, &mut second] {
            let mode: String = sqlx::query_scalar("PRAGMA journal_mode")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(mode, "wal");
            let synchronous: i64 = sqlx::query_scalar("PRAGMA synchronous")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(synchronous, 1, "1 = NORMAL");
            let busy: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(busy, 5000);
            let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
                .fetch_one(&mut **conn)
                .await
                .unwrap();
            assert_eq!(foreign_keys, 1);
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_error_without_create() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_pool(&temp_url(&dir, "nope.db"), 1, false)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("nope.db"), "{err:#}");
        assert!(!dir.path().join("nope.db").exists());
    }

    #[tokio::test]
    async fn create_makes_the_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("a/b/t.db").display());
        open_pool(&url, 1, true).await.unwrap();
        assert!(dir.path().join("a/b/t.db").exists());
    }

    #[tokio::test]
    async fn check_passes_on_a_healthy_database() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_pool(&temp_url(&dir, "t.db"), 1, true).await.unwrap();
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        let report = check(&pool).await.unwrap();
        assert!(report.is_ok(), "{report:?}");
    }

    #[tokio::test]
    async fn check_reports_broken_references() {
        let dir = tempfile::tempdir().unwrap();
        // One connection, so the PRAGMA below and the insert share it.
        let pool = open_pool(&temp_url(&dir, "t.db"), 1, true).await.unwrap();
        for sql in [
            "CREATE TABLE parent (id INTEGER PRIMARY KEY)",
            "CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER REFERENCES parent(id))",
            "PRAGMA foreign_keys = OFF",
            "INSERT INTO child (id, parent_id) VALUES (1, 42)",
            "PRAGMA foreign_keys = ON",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        let report = check(&pool).await.unwrap();
        assert!(!report.is_ok());
        assert!(report.integrity_errors.is_empty());
        assert_eq!(
            report.foreign_key_errors,
            vec!["child row 1 points to a missing row in parent".to_owned()]
        );
    }
}

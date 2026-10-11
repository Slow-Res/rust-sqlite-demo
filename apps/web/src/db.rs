//! SQLite connection pool, migrations, and the adapter-agnostic repository error.

use std::str::FromStr;
use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

use crate::config::Config;

const MAX_CONNECTIONS: u32 = 8;

/// Open the pool, creating the database file and its parent directory if needed.
/// The pragmas are part of the connect options, so they apply to *every*
/// connection the pool opens (not just the first).
pub async fn connect(config: &Config) -> anyhow::Result<SqlitePool> {
    use anyhow::Context;

    let options = SqliteConnectOptions::from_str(&config.database_url)
        .with_context(|| format!("invalid DATABASE_URL `{}`", config.database_url))?
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5));

    if let Some(dir) = options
        .get_filename()
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
    {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating database directory {}", dir.display()))?;
    }

    SqlitePoolOptions::new()
        .max_connections(MAX_CONNECTIONS)
        .connect_with(options)
        .await
        .context("connecting to the database")
}

/// Apply the migrations embedded in the binary at compile time.
pub async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::migrate!("../../migrations/sqlite").run(pool).await?;
    Ok(())
}

/// Errors a repository can report. Deliberately free of `sqlx` types in its
/// public shape so the service layer stays storage-agnostic (spec §5.1).
#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    /// A UNIQUE constraint was violated; `constraint` is e.g. `users.username`.
    #[error("unique constraint violated: {constraint}")]
    Conflict { constraint: String },
    #[error(transparent)]
    Unexpected(#[from] anyhow::Error),
}

// Implementing `From` is what makes `?` convert `sqlx::Error` into `RepoError`
// automatically inside repository methods.
impl From<sqlx::Error> for RepoError {
    fn from(err: sqlx::Error) -> Self {
        if let Some(db_err) = err.as_database_error()
            && db_err.is_unique_violation()
        {
            // SQLite reports `UNIQUE constraint failed: table.column`; sqlx's
            // `constraint()` is only populated for Postgres, so parse the message.
            let message = db_err.message();
            let constraint = message
                .split_once("constraint failed: ")
                .map_or(message, |(_, rest)| rest)
                .to_owned();
            return Self::Conflict { constraint };
        }
        Self::Unexpected(err.into())
    }
}

/// Timestamps are stored as INTEGER Unix seconds (UTC).
pub fn to_timestamp(at: DateTime<Utc>) -> i64 {
    at.timestamp()
}

pub fn from_timestamp(secs: i64) -> Result<DateTime<Utc>, RepoError> {
    DateTime::from_timestamp(secs, 0)
        .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("timestamp {secs} out of range")))
}

/// Escape `%`, `_` and `\` so user text matches literally inside `LIKE ... ESCAPE '\'`.
pub(crate) fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_round_trip() {
        let now = DateTime::from_timestamp(1_800_000_123, 0).unwrap();
        assert_eq!(from_timestamp(to_timestamp(now)).unwrap(), now);
    }

    #[test]
    fn out_of_range_timestamp_is_an_error() {
        assert!(matches!(
            from_timestamp(i64::MAX),
            Err(RepoError::Unexpected(_))
        ));
    }

    #[test]
    fn escape_like_escapes_wildcards_and_backslash() {
        assert_eq!(escape_like("100%_done\\"), "100\\%\\_done\\\\");
        assert_eq!(escape_like("plain text"), "plain text");
        assert_eq!(escape_like(""), "");
    }

    #[sqlx::test(migrations = "../../migrations/sqlite")]
    async fn unique_violation_maps_to_conflict(pool: SqlitePool) {
        let insert =
            "INSERT INTO members (name, email, created_at, updated_at) VALUES ('a', 'a@b.c', 0, 0)";
        sqlx::query(insert).execute(&pool).await.unwrap();
        let err: RepoError = sqlx::query(insert).execute(&pool).await.unwrap_err().into();
        match err {
            RepoError::Conflict { constraint } => assert_eq!(constraint, "members.email"),
            other => panic!("expected Conflict, got {other:?}"),
        }
    }
}

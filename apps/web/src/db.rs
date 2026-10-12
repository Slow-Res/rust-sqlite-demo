//! Database pools (SQLite, or Postgres behind the `postgres` feature),
//! migrations, and the adapter-agnostic repository error.

use anyhow::bail;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
#[cfg(feature = "postgres")]
use sqlx::{PgPool, postgres::PgPoolOptions};

use crate::config::Config;

// Ten connections per process on both backends, the pool size of the benchmark
// the load test follows, so the comparison measures the databases, not the pools.
// The box has 2 cores: past a handful of connections more of them only queue
// inside the database (and in Postgres each one is a process with its own memory).
// `DATABASE_MAX_CONNECTIONS` overrides it, for both backends.
const SQLITE_MAX_CONNECTIONS: u32 = 10;
#[cfg(feature = "postgres")]
const POSTGRES_MAX_CONNECTIONS: u32 = 10;

/// The backend this process runs on, chosen by the `DATABASE_URL` scheme.
/// SQLite is the product; Postgres exists only so the load test can compare
/// the two (see the README).
#[derive(Debug, Clone)]
pub enum Database {
    Sqlite(SqlitePool),
    #[cfg(feature = "postgres")]
    Postgres(PgPool),
}

impl From<SqlitePool> for Database {
    fn from(pool: SqlitePool) -> Self {
        Self::Sqlite(pool)
    }
}

impl Database {
    /// A round-trip to the database, for the health check.
    pub async fn ping(&self) -> Result<(), sqlx::Error> {
        match self {
            Self::Sqlite(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop),
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => sqlx::query("SELECT 1").execute(pool).await.map(drop),
        }
    }

    pub async fn close(&self) {
        match self {
            Self::Sqlite(pool) => pool.close().await,
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => pool.close().await,
        }
    }

    /// The SQLite pool, or an error naming the command that needs it (`seed`
    /// and `check-db` only know SQLite).
    #[cfg_attr(not(feature = "postgres"), allow(unused_variables))]
    pub fn require_sqlite(&self, command: &str) -> anyhow::Result<&SqlitePool> {
        match self {
            Self::Sqlite(pool) => Ok(pool),
            #[cfg(feature = "postgres")]
            Self::Postgres(_) => bail!("`{command}` supports only SQLite, not Postgres"),
        }
    }
}

pub(crate) fn is_postgres_url(url: &str) -> bool {
    url.starts_with("postgres:") || url.starts_with("postgresql:")
}

/// Open the pool for the backend `DATABASE_URL` names. SQLite creates the
/// database file and its parent directory if needed; its settings (WAL, busy
/// timeout, foreign keys...) come from `sqlite-setup`, the one place they are
/// defined (spec §5).
pub async fn connect(config: &Config) -> anyhow::Result<Database> {
    if is_postgres_url(&config.database_url) {
        #[cfg(feature = "postgres")]
        {
            let pool = PgPoolOptions::new()
                .max_connections(
                    config
                        .database_max_connections
                        .unwrap_or(POSTGRES_MAX_CONNECTIONS),
                )
                .connect(&config.database_url)
                .await?;
            return Ok(Database::Postgres(pool));
        }
        #[cfg(not(feature = "postgres"))]
        bail!(
            "DATABASE_URL names Postgres, but this binary was built without the `postgres` feature"
        );
    }
    let max_connections = config
        .database_max_connections
        .unwrap_or(SQLITE_MAX_CONNECTIONS);
    let pool = sqlite_setup::open_pool(&config.database_url, max_connections, true).await?;
    Ok(Database::Sqlite(pool))
}

/// Apply the migrations embedded in the binary at compile time (the SQLite or
/// the Postgres set, to match the backend).
/// `ignore_missing`: the previous version must keep running after an expand
/// migration (spec S10).
pub async fn migrate(db: &Database) -> anyhow::Result<()> {
    match db {
        Database::Sqlite(pool) => {
            let mut migrator = sqlx::migrate!("../../migrations/sqlite");
            migrator.set_ignore_missing(true);
            migrator.run(pool).await?;
        }
        #[cfg(feature = "postgres")]
        Database::Postgres(pool) => {
            // Not `migrations/postgres`: that is the central mirror the sync worker uses.
            let mut migrator = sqlx::migrate!("../../migrations/web-postgres");
            migrator.set_ignore_missing(true);
            migrator.run(pool).await?;
        }
    }
    Ok(())
}

/// Postgres reports the violated constraint by name (SQLite does not; see
/// below). Translate each name from `migrations/web-postgres` to the
/// `table.column` string SQLite produces, which is what the services compare.
fn postgres_constraint(name: &str) -> &str {
    match name {
        "uq_users_username" => "users.username",
        "uq_books_isbn" => "books.isbn",
        "uq_members_email" => "members.email",
        other => other,
    }
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
            let constraint = match db_err.constraint() {
                Some(name) => postgres_constraint(name).to_owned(),
                None => {
                    let message = db_err.message();
                    message
                        .split_once("constraint failed: ")
                        .map_or(message, |(_, rest)| rest)
                        .to_owned()
                }
            };
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

/// The words of a book search `q`: runs of letters and digits, so whitespace and
/// punctuation (including `"`, `*`, `:`, `(`, `-` and `_`) separate words and none
/// of them reaches a query. A book matches when every word is a prefix of some
/// word of its title, author or ISBN, the same on both backends.
pub(crate) fn search_words(q: &str) -> Vec<&str> {
    q.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

/// `q` with spaces and hyphens removed, when that can be the start of an ISBN
/// and differs from `q` (ISBNs are stored without separators, so "978-0-261"
/// finds "9780261..."). `None` when `q` is a single word already (the word
/// search covers it) or holds anything but ASCII letters and digits.
pub(crate) fn isbn_prefix(q: &str) -> Option<String> {
    let digits: String = q.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
    (digits != q && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_alphanumeric()))
        .then_some(digits)
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

    #[test]
    fn search_words_split_on_everything_but_letters_and_digits() {
        assert_eq!(search_words("J.R.R. Tolkien"), ["J", "R", "R", "Tolkien"]);
        assert_eq!(search_words("a_b \"c\" d*:(-e"), ["a", "b", "c", "d", "e"]);
        assert_eq!(search_words("Café Müller 12"), ["Café", "Müller", "12"]);
        assert!(search_words("%%% --- \"").is_empty());
        assert!(search_words("").is_empty());
    }

    #[test]
    fn isbn_prefix_is_q_without_separators() {
        assert_eq!(
            isbn_prefix("978-0-261 10221-7").as_deref(),
            Some("9780261102217")
        );
        assert_eq!(isbn_prefix("0441 172").as_deref(), Some("0441172"));
        assert_eq!(isbn_prefix("9780261"), None); // one word: the word search covers it
        assert_eq!(isbn_prefix("- -"), None); // nothing left
        assert_eq!(isbn_prefix("a.b c"), None); // cannot be in an ISBN
    }

    #[test]
    fn postgres_constraint_names_map_to_the_sqlite_strings() {
        assert_eq!(postgres_constraint("uq_users_username"), "users.username");
        assert_eq!(postgres_constraint("uq_books_isbn"), "books.isbn");
        assert_eq!(postgres_constraint("uq_members_email"), "members.email");
        assert_eq!(postgres_constraint("something_else"), "something_else");
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

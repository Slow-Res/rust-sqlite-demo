//! SQLite adapter for [`BookRepository`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::Availability;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};

use super::domain::BookFields;
use super::repo::{BookFilter, BookRecord, BookRepository, DeleteBookOutcome, UpdateBookOutcome};
use crate::db::{RepoError, escape_like, from_timestamp, to_timestamp};
use crate::pagination::PageParams;

// SQL fragments shared by several queries. They are macros (not `const`s)
// because `concat!` can only join literals, and sqlx 0.9 only accepts
// `&'static str` as SQL, which keeps user input out of query text.

/// Number of unreturned loans of the book aliased `b`.
macro_rules! active_loans {
    () => {
        "(SELECT COUNT(*) FROM loans l WHERE l.book_id = b.id AND l.returned_at IS NULL)"
    };
}

/// `SELECT ... FROM books b`, shaped like [`BookRow`].
macro_rules! select_books {
    () => {
        concat!(
            "SELECT b.id, b.title, b.author, b.isbn, b.published_year, b.total_copies, ",
            "b.total_copies - ",
            active_loans!(),
            " AS available_copies, b.created_at, b.updated_at FROM books b"
        )
    };
}

#[derive(Debug, Clone)]
pub struct SqliteBookRepository {
    pool: SqlitePool,
}

impl SqliteBookRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

/// A row as SQLite returns it (raw integers for timestamps and years), before
/// conversion into the adapter-agnostic [`BookRecord`].
#[derive(sqlx::FromRow)]
struct BookRow {
    id: i64,
    title: String,
    author: String,
    isbn: Option<String>,
    published_year: Option<i64>,
    total_copies: i64,
    available_copies: i64,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<BookRow> for BookRecord {
    type Error = RepoError;

    fn try_from(row: BookRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.id,
            title: row.title,
            author: row.author,
            isbn: row.isbn,
            published_year: row
                .published_year
                .map(i32::try_from)
                .transpose()
                .map_err(|err| RepoError::Unexpected(err.into()))?,
            total_copies: row.total_copies,
            available_copies: row.available_copies,
            created_at: from_timestamp(row.created_at)?,
            updated_at: from_timestamp(row.updated_at)?,
        })
    }
}

/// Append the `WHERE` clause for `filter`. Used for both the page query and the
/// count query so they can never disagree.
///
/// `QueryBuilder` is how you build SQL whose *shape* varies at runtime. User
/// text goes through `push_bind` (a `?` placeholder sent separately from the
/// SQL), so it can never be interpreted as SQL. Formatting it into the string
/// with `format!` would be an SQL injection hole.
fn push_filter(builder: &mut QueryBuilder<Sqlite>, filter: &BookFilter) {
    builder.push(" WHERE 1 = 1");
    if let Some(q) = &filter.q {
        let pattern = format!("%{}%", escape_like(q));
        builder.push(" AND (b.title LIKE ");
        builder.push_bind(pattern.clone());
        builder.push(" ESCAPE '\\' OR b.author LIKE ");
        builder.push_bind(pattern.clone());
        builder.push(" ESCAPE '\\' OR b.isbn LIKE ");
        builder.push_bind(pattern);
        builder.push(" ESCAPE '\\'");
        // ISBNs are stored without spaces or hyphens, so also match `q` with
        // those removed. Skipped when nothing is left, or `%%` would match all.
        let digits: String = q.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
        if !digits.is_empty() {
            builder.push(" OR b.isbn LIKE ");
            builder.push_bind(format!("%{}%", escape_like(&digits)));
            builder.push(" ESCAPE '\\'");
        }
        builder.push(")");
    }
    match filter.availability {
        Availability::All => {}
        Availability::Available => {
            builder.push(concat!(" AND b.total_copies - ", active_loans!(), " > 0"));
        }
        Availability::OnLoan => {
            builder.push(concat!(" AND ", active_loans!(), " > 0"));
        }
    }
}

/// Fetch one book. Takes a connection so it works both on the pool and inside
/// a transaction.
async fn fetch_book(conn: &mut SqliteConnection, id: i64) -> Result<Option<BookRecord>, RepoError> {
    let row = sqlx::query_as::<_, BookRow>(concat!(select_books!(), " WHERE b.id = ?"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(BookRecord::try_from).transpose()
}

#[async_trait]
impl BookRepository for SqliteBookRepository {
    async fn list(
        &self,
        filter: &BookFilter,
        page: PageParams,
    ) -> Result<(Vec<BookRecord>, u64), RepoError> {
        let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM books b");
        push_filter(&mut count, filter);
        let total: i64 = count.build_query_scalar().fetch_one(&self.pool).await?;

        let mut select = QueryBuilder::<Sqlite>::new(select_books!());
        push_filter(&mut select, filter);
        select.push(" ORDER BY b.title ASC, b.id ASC LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        let rows = select
            .build_query_as::<BookRow>()
            .fetch_all(&self.pool)
            .await?;

        let items = rows
            .into_iter()
            .map(BookRecord::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        let total = u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()))?;
        Ok((items, total))
    }

    async fn get(&self, id: i64) -> Result<Option<BookRecord>, RepoError> {
        let mut conn = self.pool.acquire().await?;
        fetch_book(&mut conn, id).await
    }

    async fn create(
        &self,
        fields: &BookFields,
        now: DateTime<Utc>,
    ) -> Result<BookRecord, RepoError> {
        let isbn = fields.isbn.as_ref().map(|isbn| isbn.as_str());
        let at = to_timestamp(now);
        let result = sqlx::query!(
            "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            fields.title,
            fields.author,
            isbn,
            fields.published_year,
            fields.total_copies,
            at,
            at,
        )
        .execute(&self.pool)
        .await?;
        let mut conn = self.pool.acquire().await?;
        fetch_book(&mut conn, result.last_insert_rowid())
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("inserted book vanished")))
    }

    async fn update_checked(
        &self,
        id: i64,
        fields: &BookFields,
        now: DateTime<Utc>,
    ) -> Result<UpdateBookOutcome, RepoError> {
        // The check (active loans vs. new copy count) and the write must not be
        // separated by another writer. SQLite's default `BEGIN` is deferred: it
        // only takes the write lock at the first write, so a loan could slip in
        // between our read and our update. `BEGIN IMMEDIATE` takes the write
        // lock up front, making read-then-write atomic.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let Some(current) = fetch_book(&mut tx, id).await? else {
            return Ok(UpdateBookOutcome::NotFound);
        };
        let active = current.total_copies - current.available_copies;
        if fields.total_copies < active {
            return Ok(UpdateBookOutcome::CopiesBelowActiveLoans);
        }

        let isbn = fields.isbn.as_ref().map(|isbn| isbn.as_str());
        let at = to_timestamp(now);
        sqlx::query!(
            "UPDATE books
             SET title = ?, author = ?, isbn = ?, published_year = ?, total_copies = ?, updated_at = ?
             WHERE id = ?",
            fields.title,
            fields.author,
            isbn,
            fields.published_year,
            fields.total_copies,
            at,
            id,
        )
        .execute(&mut *tx)
        .await?;

        let updated = fetch_book(&mut tx, id)
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("updated book vanished")))?;
        tx.commit().await?;
        Ok(UpdateBookOutcome::Updated(updated))
    }

    async fn delete_checked(&self, id: i64) -> Result<DeleteBookOutcome, RepoError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let exists = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM books WHERE id = ?"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if exists == 0 {
            return Ok(DeleteBookOutcome::NotFound);
        }
        let active = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM loans WHERE book_id = ? AND returned_at IS NULL"#,
            id
        )
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            return Ok(DeleteBookOutcome::HasActiveLoans);
        }

        // Returned loans disappear with the book (ON DELETE CASCADE).
        sqlx::query!("DELETE FROM books WHERE id = ?", id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(DeleteBookOutcome::Deleted)
    }
}

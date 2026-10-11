//! Postgres adapter for [`BookRepository`] (load-test comparison only).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::Availability;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};

use super::domain::BookFields;
use super::repo::{BookFilter, BookRecord, BookRepository, DeleteBookOutcome, UpdateBookOutcome};
use crate::count_cache::CountCache;
use crate::db::{RepoError, escape_like, from_timestamp, to_timestamp};
use crate::pagination::PageParams;

// SQL fragments shared by several queries; macros for the same reason as in
// `books::sqlite` (`concat!` needs literals, sqlx wants `&'static str`).

/// Number of unreturned loans of the book aliased `b`.
macro_rules! active_loans {
    () => {
        "(SELECT COUNT(*) FROM loans l WHERE l.book_id = b.id AND l.returned_at IS NULL)"
    };
}

/// The column list of [`BookRow`], including the correlated subquery.
macro_rules! book_columns {
    () => {
        concat!(
            "SELECT b.id, b.title, b.author, b.isbn, b.published_year, b.total_copies, ",
            "b.total_copies - ",
            active_loans!(),
            " AS available_copies, b.created_at, b.updated_at"
        )
    };
}

/// `SELECT ... FROM books b`, shaped like [`BookRow`].
macro_rules! select_books {
    () => {
        concat!(book_columns!(), " FROM books b")
    };
}

#[derive(Debug, Clone)]
pub struct PostgresBookRepository {
    pool: PgPool,
    counts: Arc<CountCache>,
}

impl PostgresBookRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            // Off until `with_count_cache`: totals stay exact.
            counts: Arc::new(CountCache::new(Duration::ZERO)),
        }
    }

    /// Reuse the totals of unfiltered lists through `counts` (see `count_cache`).
    pub fn with_count_cache(mut self, counts: Arc<CountCache>) -> Self {
        self.counts = counts;
        self
    }
}

/// A row as Postgres returns it (raw integers for timestamps and years), before
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

/// Append the `WHERE` clause for `filter`, for both the page and the count query.
/// Same four branches as the SQLite `LIKE` version, with `ILIKE` ("C" collation:
/// ASCII case folding only, like SQLite); the trigram GIN indexes serve them.
fn push_filter(builder: &mut QueryBuilder<Postgres>, filter: &BookFilter) {
    builder.push(" WHERE 1 = 1");
    if let Some(q) = &filter.q {
        let pattern = format!("%{}%", escape_like(q));
        builder.push(" AND (b.title ILIKE ");
        builder.push_bind(pattern.clone());
        builder.push(" ESCAPE '\\' OR b.author ILIKE ");
        builder.push_bind(pattern.clone());
        builder.push(" ESCAPE '\\' OR b.isbn ILIKE ");
        builder.push_bind(pattern);
        builder.push(" ESCAPE '\\'");
        // ISBNs are stored without spaces or hyphens, so also match `q` with
        // those removed. Skipped when nothing is left, or `%%` would match all.
        let digits: String = q.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
        if !digits.is_empty() {
            builder.push(" OR b.isbn ILIKE ");
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

/// Number of books matching `filter`.
async fn count_books(pool: &PgPool, filter: &BookFilter) -> Result<u64, RepoError> {
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM books b");
    push_filter(&mut count, filter);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;
    u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()))
}

/// Fetch one book. Takes a connection so it works both on the pool and inside
/// a transaction.
async fn fetch_book(conn: &mut PgConnection, id: i64) -> Result<Option<BookRecord>, RepoError> {
    let row = sqlx::query_as::<_, BookRow>(concat!(select_books!(), " WHERE b.id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(BookRecord::try_from).transpose()
}

#[async_trait]
impl BookRepository for PostgresBookRepository {
    async fn list(
        &self,
        filter: &BookFilter,
        page: PageParams,
    ) -> Result<(Vec<BookRecord>, u64), RepoError> {
        let total = self
            .counts
            .get_or_count_opt(filter.count_key(), || count_books(&self.pool, filter))
            .await?;

        // Deferred join: the inner query walks the index and skips OFFSET rows
        // reading only ids; the columns and the correlated subquery are computed
        // for the page's rows only, not for every skipped row.
        let mut select = QueryBuilder::<Postgres>::new(concat!(
            book_columns!(),
            " FROM (SELECT b.id FROM books b"
        ));
        push_filter(&mut select, filter);
        select.push(" ORDER BY b.title ASC, b.id ASC LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        select.push(") p JOIN books b ON b.id = p.id ORDER BY b.title ASC, b.id ASC");
        let rows = select
            .build_query_as::<BookRow>()
            .fetch_all(&self.pool)
            .await?;

        let items = rows
            .into_iter()
            .map(BookRecord::try_from)
            .collect::<Result<Vec<_>, _>>()?;
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
        let at = to_timestamp(now);
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id",
        )
        .bind(&fields.title)
        .bind(&fields.author)
        .bind(fields.isbn.as_ref().map(|isbn| isbn.as_str()))
        .bind(fields.published_year.map(i64::from))
        .bind(fields.total_copies)
        .bind(at)
        .bind(at)
        .fetch_one(&self.pool)
        .await?;
        let mut conn = self.pool.acquire().await?;
        fetch_book(&mut conn, id)
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
        // separated by another writer. SQLite gets that from `BEGIN IMMEDIATE`;
        // Postgres READ COMMITTED has no such thing, so `FOR UPDATE` locks the book
        // row first. `loans::create_checked` locks the book before it counts, so a
        // loan cannot slip in between. Lock order everywhere: book, member, loan.
        let mut tx = self.pool.begin().await?;

        let locked: Option<i64> =
            sqlx::query_scalar("SELECT id FROM books WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            return Ok(UpdateBookOutcome::NotFound);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if fields.total_copies < active {
            return Ok(UpdateBookOutcome::CopiesBelowActiveLoans);
        }

        sqlx::query(
            "UPDATE books
             SET title = $1, author = $2, isbn = $3, published_year = $4, total_copies = $5, updated_at = $6
             WHERE id = $7",
        )
        .bind(&fields.title)
        .bind(&fields.author)
        .bind(fields.isbn.as_ref().map(|isbn| isbn.as_str()))
        .bind(fields.published_year.map(i64::from))
        .bind(fields.total_copies)
        .bind(to_timestamp(now))
        .bind(id)
        .execute(&mut *tx)
        .await?;

        let updated = fetch_book(&mut tx, id)
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("updated book vanished")))?;
        tx.commit().await?;
        Ok(UpdateBookOutcome::Updated(updated))
    }

    async fn delete_checked(&self, id: i64) -> Result<DeleteBookOutcome, RepoError> {
        // Same lock as `update_checked`: no loan can be created while we count.
        let mut tx = self.pool.begin().await?;

        let locked: Option<i64> =
            sqlx::query_scalar("SELECT id FROM books WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if locked.is_none() {
            return Ok(DeleteBookOutcome::NotFound);
        }
        let active: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        if active > 0 {
            return Ok(DeleteBookOutcome::HasActiveLoans);
        }

        // Returned loans disappear with the book (ON DELETE CASCADE).
        sqlx::query("DELETE FROM books WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(DeleteBookOutcome::Deleted)
    }
}

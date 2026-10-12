//! Postgres adapter for [`BookRepository`] (load-test comparison only).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::Availability;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};

use super::domain::BookFields;
use super::repo::{
    AVAILABLE_TOTAL, BookFilter, BookRecord, BookRepository, DeleteBookOutcome, ON_LOAN_TOTAL,
    UpdateBookOutcome,
};
use crate::count_cache::CountCache;
use crate::db::{RepoError, escape_like, from_timestamp, isbn_prefix, search_words, to_timestamp};
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

/// A [`BookRow`] of a word search, with the number of matches of the whole search.
#[derive(sqlx::FromRow)]
struct TotalBookRow {
    #[sqlx(flatten)]
    book: BookRow,
    total: i64,
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

/// The `to_tsquery` text for the words of a search: every word a quoted prefix
/// (`'anc':* & 'ada':*`). Words are letters and digits only (see `search_words`),
/// so none holds a quote, a backslash or a tsquery operator.
fn tsquery_text(words: &[&str]) -> String {
    let words: Vec<String> = words.iter().map(|word| format!("'{word}':*")).collect();
    words.join(" & ")
}

/// Append the `WHERE` clause for `filter`, for both the page and the count query.
/// Same meaning as the SQLite version: every word of `q` is a prefix of a word of
/// title, author or ISBN. `book_search_vector` is the expression of the GIN index
/// `idx_books_search`; the ISBN form (`q` without separators) is a `LIKE` prefix
/// that the btree on `isbn` ("C" collation) serves.
fn push_filter(builder: &mut QueryBuilder<Postgres>, filter: &BookFilter) {
    builder.push(" WHERE 1 = 1");
    if let Some(q) = &filter.q {
        let words = search_words(q);
        if words.is_empty() {
            // Nothing but punctuation: no word to look up, so match it as plain
            // text in a scan (rare, and it keeps the old result for such queries).
            let pattern = format!("%{}%", escape_like(q));
            builder.push(" AND (b.title ILIKE ");
            builder.push_bind(pattern.clone());
            builder.push(" ESCAPE '\\' OR b.author ILIKE ");
            builder.push_bind(pattern.clone());
            builder.push(" ESCAPE '\\' OR b.isbn ILIKE ");
            builder.push_bind(pattern);
            builder.push(" ESCAPE '\\'");
            // Skipped when nothing is left, or `%%` would match all.
            let digits: String = q.chars().filter(|c| !matches!(c, ' ' | '-')).collect();
            if !digits.is_empty() {
                builder.push(" OR b.isbn ILIKE ");
                builder.push_bind(format!("%{}%", escape_like(&digits)));
                builder.push(" ESCAPE '\\'");
            }
            builder.push(")");
        } else {
            builder.push(
                " AND (book_search_vector(b.title, b.author, b.isbn) @@ to_tsquery('simple', ",
            );
            builder.push_bind(tsquery_text(&words));
            builder.push(")");
            if let Some(digits) = isbn_prefix(q) {
                // Letters and digits only, so no `LIKE` wildcard; ISBNs hold a capital X.
                builder.push(" OR b.isbn LIKE ");
                builder.push_bind(format!("{}%", digits.to_ascii_uppercase()));
            }
            builder.push(")");
        }
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
    // An availability filter without a search: count the active loans once. The
    // predicate in `push_filter` runs a subquery per book, over the whole table.
    if filter.counts_from_loans() {
        let sql = match filter.availability {
            Availability::OnLoan => ON_LOAN_TOTAL,
            _ => AVAILABLE_TOTAL,
        };
        let total: i64 = sqlx::query_scalar(sql).fetch_one(pool).await?;
        return u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()));
    }
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
        after: Option<(String, i64)>,
    ) -> Result<(Vec<BookRecord>, u64), RepoError> {
        // Deferred join: the inner query walks the index and skips OFFSET rows
        // reading only ids; the columns and the correlated subquery are computed
        // for the page's rows only, not for every skipped row.
        let mut select = QueryBuilder::<Postgres>::new(book_columns!());
        // A word search finds its matches through the GIN index in a CTE that
        // the planner must not fold back into the title-order walk: the matches
        // of one word cluster in title order (titles start with an adjective),
        // so the walk-and-filter plan can read most of the table to find 21 rows.
        let by_words = filter
            .q
            .as_deref()
            .is_some_and(|q| !search_words(q).is_empty());
        if by_words {
            // The total comes from the same statement: a window over the whole
            // match set. A separate count would re-read the same matches, and
            // the count cache rarely hits for searches (many keys, each short
            // lived). The cursor therefore comes after the window, or the total
            // would only count the rows past it.
            select
                .push(", p.total FROM (WITH m AS MATERIALIZED (SELECT b.id, b.title FROM books b");
            push_filter(&mut select, filter);
            select.push(
                ") SELECT id, title, total FROM \
                 (SELECT id, title, COUNT(*) OVER () AS total FROM m) x",
            );
            if let Some((title, id)) = after {
                select.push(" WHERE (x.title, x.id) > (");
                select.push_bind(title);
                select.push(", ");
                select.push_bind(id);
                select.push(")");
            }
            select.push(" ORDER BY title ASC, id ASC");
        } else {
            select.push(" FROM (SELECT b.id FROM books b");
            push_filter(&mut select, filter);
            if let Some((title, id)) = after {
                select.push(" AND (b.title, b.id) > (");
                select.push_bind(title);
                select.push(", ");
                select.push_bind(id);
                select.push(")");
            }
            select.push(" ORDER BY b.title ASC, b.id ASC");
        }
        select.push(" LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        select.push(") p JOIN books b ON b.id = p.id ORDER BY b.title ASC, b.id ASC");

        if by_words {
            let rows = select
                .build_query_as::<TotalBookRow>()
                .fetch_all(&self.pool)
                .await?;
            if let Some(first) = rows.first() {
                let total =
                    u64::try_from(first.total).map_err(|err| RepoError::Unexpected(err.into()))?;
                let items = rows
                    .into_iter()
                    .map(|row| BookRecord::try_from(row.book))
                    .collect::<Result<Vec<_>, _>>()?;
                return Ok((items, total));
            }
            // An empty page (past the end, or no match) carries no window value:
            // count, so the total stays exact.
            let total = self
                .counts
                .get_or_count_opt(filter.count_key(), || count_books(&self.pool, filter))
                .await?;
            return Ok((Vec::new(), total));
        }

        let total = self
            .counts
            .get_or_count_opt(filter.count_key(), || count_books(&self.pool, filter))
            .await?;
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

//! SQLite adapter for [`LoanRepository`].

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::LoanStatus;
use sqlx::{QueryBuilder, Sqlite, SqliteConnection, SqlitePool};

use super::repo::{
    CreateLoanOutcome, LoanDetails, LoanFilter, LoanRepository, NewLoanRecord, ReturnLoanOutcome,
};
use crate::count_cache::CountCache;
use crate::db::{RepoError, from_timestamp, to_timestamp};
use crate::pagination::PageParams;

/// `SELECT ... FROM loans l` joined with its book and member, shaped like
/// [`LoanRow`]. A macro for the same reason as in `books::sqlite`: `concat!`
/// needs literals, and sqlx only accepts `&'static str` SQL.
macro_rules! select_loans {
    () => {
        concat!(
            "SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name, ",
            "l.borrowed_at, l.due_at, l.returned_at ",
            "FROM loans l JOIN books b ON b.id = l.book_id JOIN members m ON m.id = l.member_id"
        )
    };
}

#[derive(Debug, Clone)]
pub struct SqliteLoanRepository {
    pool: SqlitePool,
    counts: Arc<CountCache>,
}

impl SqliteLoanRepository {
    pub fn new(pool: SqlitePool) -> Self {
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

/// A row as SQLite returns it (raw integer timestamps).
#[derive(sqlx::FromRow)]
struct LoanRow {
    id: i64,
    book_id: i64,
    book_title: String,
    member_id: i64,
    member_name: String,
    borrowed_at: i64,
    due_at: i64,
    returned_at: Option<i64>,
}

impl TryFrom<LoanRow> for LoanDetails {
    type Error = RepoError;

    fn try_from(row: LoanRow) -> Result<Self, RepoError> {
        Ok(Self {
            id: row.id,
            book_id: row.book_id,
            book_title: row.book_title,
            member_id: row.member_id,
            member_name: row.member_name,
            borrowed_at: from_timestamp(row.borrowed_at)?,
            due_at: from_timestamp(row.due_at)?,
            returned_at: row.returned_at.map(from_timestamp).transpose()?,
        })
    }
}

/// Append the `WHERE` clause for `filter`, shared by the count and page queries.
/// `Overdue` uses the same rule as the service: unreturned and `due_at < now`.
fn push_filter(builder: &mut QueryBuilder<Sqlite>, filter: &LoanFilter, now: DateTime<Utc>) {
    builder.push(" WHERE 1 = 1");
    match filter.status {
        LoanStatus::Active => {
            builder.push(" AND l.returned_at IS NULL");
        }
        LoanStatus::Overdue => {
            builder.push(" AND l.returned_at IS NULL AND l.due_at < ");
            builder.push_bind(to_timestamp(now));
        }
        LoanStatus::Returned => {
            builder.push(" AND l.returned_at IS NOT NULL");
        }
        LoanStatus::All => {}
    }
    if let Some(member_id) = filter.member_id {
        builder.push(" AND l.member_id = ");
        builder.push_bind(member_id);
    }
    if let Some(book_id) = filter.book_id {
        builder.push(" AND l.book_id = ");
        builder.push_bind(book_id);
    }
}

/// Number of loans matching `filter`.
async fn count_loans(
    pool: &SqlitePool,
    filter: &LoanFilter,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let mut count = QueryBuilder::<Sqlite>::new("SELECT COUNT(*) FROM loans l");
    push_filter(&mut count, filter, now);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;
    u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()))
}

/// Fetch one loan. Takes a connection so it works both on the pool and inside
/// a transaction.
async fn fetch_loan(
    conn: &mut SqliteConnection,
    id: i64,
) -> Result<Option<LoanDetails>, RepoError> {
    let row = sqlx::query_as::<_, LoanRow>(concat!(select_loans!(), " WHERE l.id = ?"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(LoanDetails::try_from).transpose()
}

#[async_trait]
impl LoanRepository for SqliteLoanRepository {
    async fn list(
        &self,
        filter: &LoanFilter,
        now: DateTime<Utc>,
        page: PageParams,
    ) -> Result<(Vec<LoanDetails>, u64), RepoError> {
        let total = self
            .counts
            .get_or_count_opt(filter.count_key(), || count_loans(&self.pool, filter, now))
            .await?;

        // Deferred join: the inner query walks the index and skips OFFSET rows
        // reading only ids; the joins to books and members are done for the
        // page's rows only, not for every skipped row.
        let order = match filter.status {
            LoanStatus::Returned => " ORDER BY l.returned_at DESC, l.id DESC",
            _ => " ORDER BY l.due_at ASC, l.id ASC",
        };
        let mut select = QueryBuilder::<Sqlite>::new(concat!(
            "SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name, ",
            "l.borrowed_at, l.due_at, l.returned_at FROM (SELECT l.id FROM loans l"
        ));
        push_filter(&mut select, filter, now);
        select.push(order);
        select.push(" LIMIT ");
        select.push_bind(page.limit());
        select.push(" OFFSET ");
        select.push_bind(page.offset());
        select.push(
            ") p JOIN loans l ON l.id = p.id JOIN books b ON b.id = l.book_id JOIN members m ON m.id = l.member_id",
        );
        select.push(order);
        let rows = select
            .build_query_as::<LoanRow>()
            .fetch_all(&self.pool)
            .await?;

        let items = rows
            .into_iter()
            .map(LoanDetails::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok((items, total))
    }

    async fn get(&self, id: i64) -> Result<Option<LoanDetails>, RepoError> {
        let mut conn = self.pool.acquire().await?;
        fetch_loan(&mut conn, id).await
    }

    async fn create_checked(
        &self,
        loan: NewLoanRecord,
        max_active_per_member: i64,
    ) -> Result<CreateLoanOutcome, RepoError> {
        // Check-then-insert must be atomic, or two simultaneous requests could
        // both take the last copy. `BEGIN IMMEDIATE` takes SQLite's write lock
        // up front (see `books::sqlite`), so the checks below see the same data
        // the insert will be written against. Returning early drops `tx`,
        // which rolls it back.
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        // The checks run in a fixed order; the first failure is the one reported.
        // 1. The book exists (we also need its copy count).
        let Some(total_copies) =
            sqlx::query_scalar!("SELECT total_copies FROM books WHERE id = ?", loan.book_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(CreateLoanOutcome::BookNotFound);
        };

        // 2. The member exists.
        let member = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM members WHERE id = ?"#,
            loan.member_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if member == 0 {
            return Ok(CreateLoanOutcome::MemberNotFound);
        }

        // 3. A copy is free: total copies minus unreturned loans of this book.
        let on_loan = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM loans WHERE book_id = ? AND returned_at IS NULL"#,
            loan.book_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if on_loan >= total_copies {
            return Ok(CreateLoanOutcome::NoCopiesAvailable);
        }

        // 4. The member is under their limit.
        let held = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM loans WHERE member_id = ? AND returned_at IS NULL"#,
            loan.member_id
        )
        .fetch_one(&mut *tx)
        .await?;
        if held >= max_active_per_member {
            return Ok(CreateLoanOutcome::LoanLimitReached);
        }

        let borrowed_at = to_timestamp(loan.borrowed_at);
        let due_at = to_timestamp(loan.due_at);
        let result = sqlx::query!(
            "INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES (?, ?, ?, ?)",
            loan.book_id,
            loan.member_id,
            borrowed_at,
            due_at,
        )
        .execute(&mut *tx)
        .await?;

        let created = fetch_loan(&mut tx, result.last_insert_rowid())
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("inserted loan vanished")))?;
        tx.commit().await?;
        Ok(CreateLoanOutcome::Created(created))
    }

    async fn mark_returned(
        &self,
        id: i64,
        returned_at: DateTime<Utc>,
    ) -> Result<ReturnLoanOutcome, RepoError> {
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;

        let Some(current) = fetch_loan(&mut tx, id).await? else {
            return Ok(ReturnLoanOutcome::NotFound);
        };
        if current.returned_at.is_some() {
            return Ok(ReturnLoanOutcome::AlreadyReturned);
        }

        let at = to_timestamp(returned_at);
        sqlx::query!("UPDATE loans SET returned_at = ? WHERE id = ?", at, id)
            .execute(&mut *tx)
            .await?;

        let returned = fetch_loan(&mut tx, id)
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("returned loan vanished")))?;
        tx.commit().await?;
        Ok(ReturnLoanOutcome::Returned(returned))
    }
}

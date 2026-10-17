//! Postgres adapter for [`LoanRepository`] (load-test comparison only).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::LoanStatus;
use sqlx::{PgConnection, PgPool, Postgres, QueryBuilder};

use super::repo::{
    CreateLoanOutcome, LoanDetails, LoanFilter, LoanRepository, NewLoanRecord, ReturnLoanOutcome,
};
use crate::count_cache::CountCache;
use crate::db::{RepoError, from_timestamp, to_timestamp};
use crate::jobs::events::{LoanEventKind, LoanEvents};
use crate::pagination::PageParams;

/// `SELECT ... FROM loans l` joined with its book and member, shaped like
/// [`LoanRow`]. A macro for the same reason as in `books::sqlite`.
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
pub struct PostgresLoanRepository {
    pool: PgPool,
    counts: Arc<CountCache>,
    /// Jobs to enqueue on borrow and return; `None` with jobs disabled.
    events: Option<LoanEvents>,
}

impl PostgresLoanRepository {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            // Off until `with_count_cache`: totals stay exact.
            counts: Arc::new(CountCache::new(Duration::ZERO)),
            events: None,
        }
    }

    /// Reuse the totals of unfiltered lists through `counts` (see `count_cache`).
    pub fn with_count_cache(mut self, counts: Arc<CountCache>) -> Self {
        self.counts = counts;
        self
    }

    /// Enqueue loan-event jobs in the borrow and return transactions (spec §15.1).
    pub fn with_events(mut self, events: Option<LoanEvents>) -> Self {
        self.events = events;
        self
    }
}

/// A row as Postgres returns it (raw integer timestamps).
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
fn push_filter(builder: &mut QueryBuilder<Postgres>, filter: &LoanFilter, now: DateTime<Utc>) {
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
    pool: &PgPool,
    filter: &LoanFilter,
    now: DateTime<Utc>,
) -> Result<u64, RepoError> {
    let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM loans l");
    push_filter(&mut count, filter, now);
    let total: i64 = count.build_query_scalar().fetch_one(pool).await?;
    u64::try_from(total).map_err(|err| RepoError::Unexpected(err.into()))
}

/// Fetch one loan. Takes a connection so it works both on the pool and inside
/// a transaction.
async fn fetch_loan(conn: &mut PgConnection, id: i64) -> Result<Option<LoanDetails>, RepoError> {
    let row = sqlx::query_as::<_, LoanRow>(concat!(select_loans!(), " WHERE l.id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(LoanDetails::try_from).transpose()
}

#[async_trait]
impl LoanRepository for PostgresLoanRepository {
    async fn list(
        &self,
        filter: &LoanFilter,
        now: DateTime<Utc>,
        page: PageParams,
        after: Option<(i64, i64)>,
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
        let mut select = QueryBuilder::<Postgres>::new(concat!(
            "SELECT l.id, l.book_id, b.title AS book_title, l.member_id, m.name AS member_name, ",
            "l.borrowed_at, l.due_at, l.returned_at FROM (SELECT l.id FROM loans l"
        ));
        push_filter(&mut select, filter, now);
        if let Some((key, id)) = after {
            // Strictly after the cursor in the list's own direction.
            select.push(match filter.status {
                LoanStatus::Returned => " AND (l.returned_at, l.id) < (",
                _ => " AND (l.due_at, l.id) > (",
            });
            select.push_bind(key);
            select.push(", ");
            select.push_bind(id);
            select.push(")");
        }
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
        // both take the last copy. SQLite gets that from `BEGIN IMMEDIATE`. Postgres
        // READ COMMITTED does not serialize writers, so we lock the rows the checks
        // depend on with `FOR UPDATE` before counting: the book (copies free) and the
        // member (loan limit). A competing borrow blocks on the lock, then its counts
        // (each statement sees a fresh snapshot) include our committed loan.
        // Lock order everywhere is book, member, loan, so transactions cannot deadlock.
        // Returning early drops `tx`, which rolls it back and releases the locks.
        let mut tx = self.pool.begin().await?;

        // The checks run in a fixed order; the first failure is the one reported.
        // 1. The book exists (we also need its copy count).
        let Some(total_copies): Option<i64> =
            sqlx::query_scalar("SELECT total_copies FROM books WHERE id = $1 FOR UPDATE")
                .bind(loan.book_id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(CreateLoanOutcome::BookNotFound);
        };

        // 2. The member exists.
        let member: Option<i64> =
            sqlx::query_scalar("SELECT id FROM members WHERE id = $1 FOR UPDATE")
                .bind(loan.member_id)
                .fetch_optional(&mut *tx)
                .await?;
        if member.is_none() {
            return Ok(CreateLoanOutcome::MemberNotFound);
        }

        // 3. A copy is free: total copies minus unreturned loans of this book.
        let on_loan: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL",
        )
        .bind(loan.book_id)
        .fetch_one(&mut *tx)
        .await?;
        if on_loan >= total_copies {
            return Ok(CreateLoanOutcome::NoCopiesAvailable);
        }

        // 4. The member is under their limit.
        let held: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM loans WHERE member_id = $1 AND returned_at IS NULL",
        )
        .bind(loan.member_id)
        .fetch_one(&mut *tx)
        .await?;
        if held >= max_active_per_member {
            return Ok(CreateLoanOutcome::LoanLimitReached);
        }

        let id: i64 = sqlx::query_scalar(
            "INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(loan.book_id)
        .bind(loan.member_id)
        .bind(to_timestamp(loan.borrowed_at))
        .bind(to_timestamp(loan.due_at))
        .fetch_one(&mut *tx)
        .await?;

        let created = fetch_loan(&mut tx, id)
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("inserted loan vanished")))?;
        if let Some(events) = &self.events {
            events
                .record(&mut tx, LoanEventKind::Borrowed, &created)
                .await
                .map_err(|err| RepoError::Unexpected(err.into()))?;
        }
        tx.commit().await?;
        Ok(CreateLoanOutcome::Created(created))
    }

    async fn mark_returned(
        &self,
        id: i64,
        returned_at: DateTime<Utc>,
    ) -> Result<ReturnLoanOutcome, RepoError> {
        let mut tx = self.pool.begin().await?;

        // Lock order is book, member, loan (see `create_checked`), but the loan
        // tells us which book and member those are, so read it unlocked first.
        let Some((book_id, member_id)): Option<(i64, i64)> =
            sqlx::query_as("SELECT book_id, member_id FROM loans WHERE id = $1")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(ReturnLoanOutcome::NotFound);
        };
        sqlx::query("SELECT id FROM books WHERE id = $1 FOR UPDATE")
            .bind(book_id)
            .fetch_optional(&mut *tx)
            .await?;
        sqlx::query("SELECT id FROM members WHERE id = $1 FOR UPDATE")
            .bind(member_id)
            .fetch_optional(&mut *tx)
            .await?;

        // Now lock the loan and decide on its current state. It is gone if a
        // concurrent delete of its book or member removed it in the meantime.
        let Some(current): Option<Option<i64>> =
            sqlx::query_scalar("SELECT returned_at FROM loans WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?
        else {
            return Ok(ReturnLoanOutcome::NotFound);
        };
        if current.is_some() {
            return Ok(ReturnLoanOutcome::AlreadyReturned);
        }

        sqlx::query("UPDATE loans SET returned_at = $1 WHERE id = $2")
            .bind(to_timestamp(returned_at))
            .bind(id)
            .execute(&mut *tx)
            .await?;

        let returned = fetch_loan(&mut tx, id)
            .await?
            .ok_or_else(|| RepoError::Unexpected(anyhow::anyhow!("returned loan vanished")))?;
        if let Some(events) = &self.events {
            events
                .record(&mut tx, LoanEventKind::Returned, &returned)
                .await
                .map_err(|err| RepoError::Unexpected(err.into()))?;
        }
        tx.commit().await?;
        Ok(ReturnLoanOutcome::Returned(returned))
    }
}

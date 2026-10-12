//! The books port: what the service needs from storage, with no SQL in sight.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::{Availability, Book};

use super::domain::BookFields;
use crate::count_cache::CountKey;
use crate::db::RepoError;
use crate::pagination::PageParams;

/// A stored book, including the derived `available_copies`
/// (`total_copies` minus active loans).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookRecord {
    pub id: i64,
    pub title: String,
    pub author: String,
    pub isbn: Option<String>,
    pub published_year: Option<i32>,
    pub total_copies: i64,
    pub available_copies: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<BookRecord> for Book {
    fn from(record: BookRecord) -> Self {
        Self {
            id: record.id,
            title: record.title,
            author: record.author,
            isbn: record.isbn,
            published_year: record.published_year,
            total_copies: record.total_copies,
            available_copies: record.available_copies,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

// The totals of an availability-only filter, from the active loans. The per-book
// predicate (`total_copies - active loans > 0`) is a correlated subquery for every
// book: 1.7 s per request at 500k books. These two group the active loans once
// (partial index `idx_loans_active_book`) and give the same number: a book with
// no active loan is available (`books.total_copies` is at least 1, CHECK), and a
// book is full when its active loans use every copy. SQL both backends accept.

/// Books with at least one active loan.
pub const ON_LOAN_TOTAL: &str =
    "SELECT COUNT(DISTINCT book_id) FROM loans WHERE returned_at IS NULL";

/// Books with at least one free copy: all books minus the full ones.
pub const AVAILABLE_TOTAL: &str = "SELECT (SELECT COUNT(*) FROM books) - \
    (SELECT COUNT(*) FROM (SELECT book_id, COUNT(*) AS n FROM loans \
     WHERE returned_at IS NULL GROUP BY book_id) a \
     JOIN books b ON b.id = a.book_id WHERE b.total_copies - a.n <= 0)";

/// Search criteria for the book list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFilter {
    /// Already trimmed; `None` means no text search.
    pub q: Option<String>,
    pub availability: Availability,
}

impl BookFilter {
    /// The count-cache slot for this filter, if its total is cacheable: the
    /// unfiltered list, and a list filtered by availability alone (no search).
    pub fn count_key(&self) -> Option<CountKey> {
        if self.q.is_some() {
            return None;
        }
        Some(match self.availability {
            Availability::All => CountKey::Books,
            Availability::Available => CountKey::BooksAvailable,
            Availability::OnLoan => CountKey::BooksOnLoan,
        })
    }

    /// Whether the total can come from the active loans alone: an availability
    /// filter with no search. Then the adapters count with [`ON_LOAN_TOTAL`] or
    /// [`AVAILABLE_TOTAL`] instead of a subquery per book.
    pub fn counts_from_loans(&self) -> bool {
        self.q.is_none() && self.availability != Availability::All
    }
}

/// Result of `update_checked`. The adapter decides atomically; the service
/// turns each outcome into an HTTP-level error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateBookOutcome {
    Updated(BookRecord),
    NotFound,
    /// `total_copies` would drop below the number of copies currently on loan.
    CopiesBelowActiveLoans,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteBookOutcome {
    Deleted,
    NotFound,
    HasActiveLoans,
}

#[async_trait]
pub trait BookRepository: Send + Sync {
    /// Up to `page.limit()` (`per_page + 1`) matching books plus the total number
    /// of matches. With `after` (the `(title, id)` of the last book seen) the rows
    /// start strictly after it and `page.offset()` is 0.
    async fn list(
        &self,
        filter: &BookFilter,
        page: PageParams,
        after: Option<(String, i64)>,
    ) -> Result<(Vec<BookRecord>, u64), RepoError>;

    async fn get(&self, id: i64) -> Result<Option<BookRecord>, RepoError>;

    /// Insert a book. A duplicate ISBN is reported as `RepoError::Conflict`.
    async fn create(
        &self,
        fields: &BookFields,
        now: DateTime<Utc>,
    ) -> Result<BookRecord, RepoError>;

    /// Replace all fields, unless that would leave fewer copies than active loans.
    /// A duplicate ISBN is reported as `RepoError::Conflict`.
    async fn update_checked(
        &self,
        id: i64,
        fields: &BookFields,
        now: DateTime<Utc>,
    ) -> Result<UpdateBookOutcome, RepoError>;

    /// Delete a book (and its returned loans) unless it has active loans.
    async fn delete_checked(&self, id: i64) -> Result<DeleteBookOutcome, RepoError>;
}

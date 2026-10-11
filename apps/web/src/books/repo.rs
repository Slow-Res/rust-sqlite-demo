//! The books port: what the service needs from storage, with no SQL in sight.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::{Availability, Book};

use super::domain::BookFields;
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

/// Search criteria for the book list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookFilter {
    /// Already trimmed; `None` means no text search.
    pub q: Option<String>,
    pub availability: Availability,
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
    /// One page of matching books plus the total number of matches.
    async fn list(
        &self,
        filter: &BookFilter,
        page: PageParams,
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

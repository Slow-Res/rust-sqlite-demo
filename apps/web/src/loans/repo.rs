//! The loans port: what the service needs from storage, with no SQL in sight.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::LoanStatus;

use crate::db::RepoError;
use crate::pagination::PageParams;

/// A stored loan joined with the book title and member name it refers to.
/// `is_overdue` is deliberately absent: it depends on "now", so the service
/// computes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoanDetails {
    pub id: i64,
    pub book_id: i64,
    pub book_title: String,
    pub member_id: i64,
    pub member_name: String,
    pub borrowed_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
    pub returned_at: Option<DateTime<Utc>>,
}

/// Criteria for the loan list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoanFilter {
    pub status: LoanStatus,
    pub member_id: Option<i64>,
    pub book_id: Option<i64>,
}

/// Everything needed to insert a loan; the service has already decided the dates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewLoanRecord {
    pub book_id: i64,
    pub member_id: i64,
    pub borrowed_at: DateTime<Utc>,
    pub due_at: DateTime<Utc>,
}

/// Result of `create_checked`. The variants are listed in the order the
/// adapter checks them, so a request that breaks several rules gets the first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreateLoanOutcome {
    Created(LoanDetails),
    BookNotFound,
    MemberNotFound,
    NoCopiesAvailable,
    LoanLimitReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnLoanOutcome {
    Returned(LoanDetails),
    NotFound,
    AlreadyReturned,
}

#[async_trait]
pub trait LoanRepository: Send + Sync {
    /// One page of matching loans plus the total number of matches. `now` is
    /// only used by the `Overdue` filter (`due_at < now`).
    async fn list(
        &self,
        filter: &LoanFilter,
        now: DateTime<Utc>,
        page: PageParams,
    ) -> Result<(Vec<LoanDetails>, u64), RepoError>;

    async fn get(&self, id: i64) -> Result<Option<LoanDetails>, RepoError>;

    /// Atomically check book, member, free copies and the member's active-loan
    /// limit (`max_active_per_member`), then insert the loan.
    async fn create_checked(
        &self,
        loan: NewLoanRecord,
        max_active_per_member: i64,
    ) -> Result<CreateLoanOutcome, RepoError>;

    /// Atomically mark a loan as returned, unless it already was.
    async fn mark_returned(
        &self,
        id: i64,
        returned_at: DateTime<Utc>,
    ) -> Result<ReturnLoanOutcome, RepoError>;
}

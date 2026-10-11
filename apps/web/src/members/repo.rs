//! The members port: what the service needs from storage, with no SQL in sight.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use library_shared::Member;

use super::domain::MemberFields;
use crate::db::RepoError;
use crate::pagination::PageParams;

/// A stored member, including the derived `active_loans` (unreturned loans).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberRecord {
    pub id: i64,
    pub name: String,
    pub email: String,
    pub active_loans: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl From<MemberRecord> for Member {
    fn from(record: MemberRecord) -> Self {
        Self {
            id: record.id,
            name: record.name,
            email: record.email,
            active_loans: record.active_loans,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

/// Search criteria for the member list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFilter {
    /// Already trimmed; `None` means no text search.
    pub q: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMemberOutcome {
    Deleted,
    NotFound,
    HasActiveLoans,
}

#[async_trait]
pub trait MemberRepository: Send + Sync {
    /// One page of matching members plus the total number of matches.
    async fn list(
        &self,
        filter: &MemberFilter,
        page: PageParams,
    ) -> Result<(Vec<MemberRecord>, u64), RepoError>;

    async fn get(&self, id: i64) -> Result<Option<MemberRecord>, RepoError>;

    /// Insert a member. A duplicate email is reported as `RepoError::Conflict`.
    async fn create(
        &self,
        fields: &MemberFields,
        now: DateTime<Utc>,
    ) -> Result<MemberRecord, RepoError>;

    /// Replace all fields; `None` if the member does not exist.
    /// A duplicate email is reported as `RepoError::Conflict`.
    async fn update(
        &self,
        id: i64,
        fields: &MemberFields,
        now: DateTime<Utc>,
    ) -> Result<Option<MemberRecord>, RepoError>;

    /// Delete a member (and their returned loans) unless they have active loans.
    async fn delete_checked(&self, id: i64) -> Result<DeleteMemberOutcome, RepoError>;
}

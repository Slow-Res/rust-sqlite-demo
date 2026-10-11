//! The auth port: what the service needs from storage, with no SQL in sight.

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use crate::db::RepoError;

/// A user row including the secret hash; only the login path ever sees this.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserRecord {
    pub id: i64,
    pub username: String,
    pub password_hash: String,
}

/// The user a valid session belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionUser {
    pub id: i64,
    pub username: String,
}

// A trait with `async fn` that must be used as `dyn AuthRepository` needs the
// `async_trait` macro: native async-fn-in-trait is not yet dyn-compatible.
#[async_trait]
pub trait AuthRepository: Send + Sync {
    /// Insert a user and return its id. A duplicate username (case-insensitive)
    /// is reported as `RepoError::Conflict`.
    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, RepoError>;

    async fn find_user_by_username(&self, username: &str) -> Result<Option<UserRecord>, RepoError>;

    async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), RepoError>;

    /// The user for an *unexpired* session with this token hash.
    async fn find_session_user(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SessionUser>, RepoError>;

    async fn delete_session(&self, token_hash: &str) -> Result<(), RepoError>;

    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), RepoError>;
}

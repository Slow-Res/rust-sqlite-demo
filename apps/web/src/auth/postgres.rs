//! Postgres adapter for [`AuthRepository`] (load-test comparison only).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use super::repo::{AuthRepository, SessionUser, UserRecord};
use crate::db::{RepoError, to_timestamp};

#[derive(Debug, Clone)]
pub struct PostgresAuthRepository {
    pool: PgPool,
}

impl PostgresAuthRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

// Runtime queries only: there is no Postgres to check `query!` against at compile time.
#[async_trait]
impl AuthRepository for PostgresAuthRepository {
    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, RepoError> {
        let id = sqlx::query_scalar(
            "INSERT INTO users (username, password_hash, created_at) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(username)
        .bind(password_hash)
        .bind(to_timestamp(now))
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    async fn find_user_by_username(&self, username: &str) -> Result<Option<UserRecord>, RepoError> {
        // SQLite declares `username` COLLATE NOCASE; here the unique index is on
        // `lower(username)` ("C" collation, so ASCII folding only), and so is this match.
        let row: Option<(i64, String, String)> = sqlx::query_as(
            "SELECT id, username, password_hash FROM users WHERE lower(username) = lower($1)",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, username, password_hash)| UserRecord {
            id,
            username,
            password_hash,
        }))
    }

    async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES ($1, $2, $3, $4)",
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(to_timestamp(now))
        .bind(to_timestamp(expires_at))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_session_user(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SessionUser>, RepoError> {
        let row: Option<(i64, String)> = sqlx::query_as(
            "SELECT u.id, u.username
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = $1 AND s.expires_at > $2",
        )
        .bind(token_hash)
        .bind(to_timestamp(now))
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, username)| SessionUser { id, username }))
    }

    async fn delete_session(&self, token_hash: &str) -> Result<(), RepoError> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = $1")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), RepoError> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= $1")
            .bind(to_timestamp(now))
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

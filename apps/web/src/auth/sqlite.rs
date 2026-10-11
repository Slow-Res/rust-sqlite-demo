//! SQLite adapter for [`AuthRepository`].

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;

use super::repo::{AuthRepository, SessionUser, UserRecord};
use crate::db::{RepoError, to_timestamp};

#[derive(Debug, Clone)]
pub struct SqliteAuthRepository {
    pool: SqlitePool,
}

impl SqliteAuthRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

// `query!` checks the SQL against the dev database at compile time; the
// results are cached in `backend/.sqlx/` so CI can build offline.
#[async_trait]
impl AuthRepository for SqliteAuthRepository {
    async fn create_user(
        &self,
        username: &str,
        password_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<i64, RepoError> {
        let created_at = to_timestamp(now);
        let result = sqlx::query!(
            "INSERT INTO users (username, password_hash, created_at) VALUES (?, ?, ?)",
            username,
            password_hash,
            created_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(result.last_insert_rowid())
    }

    async fn find_user_by_username(&self, username: &str) -> Result<Option<UserRecord>, RepoError> {
        // `username` is declared `COLLATE NOCASE`, so this match ignores case.
        let row = sqlx::query!(
            r#"SELECT id AS "id!", username, password_hash FROM users WHERE username = ?"#,
            username,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| UserRecord {
            id: r.id,
            username: r.username,
            password_hash: r.password_hash,
        }))
    }

    async fn create_session(
        &self,
        token_hash: &str,
        user_id: i64,
        now: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    ) -> Result<(), RepoError> {
        let created_at = to_timestamp(now);
        let expires_at = to_timestamp(expires_at);
        sqlx::query!(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES (?, ?, ?, ?)",
            token_hash,
            user_id,
            created_at,
            expires_at,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn find_session_user(
        &self,
        token_hash: &str,
        now: DateTime<Utc>,
    ) -> Result<Option<SessionUser>, RepoError> {
        let now = to_timestamp(now);
        let row = sqlx::query!(
            r#"SELECT u.id AS "id!", u.username
               FROM sessions s JOIN users u ON u.id = s.user_id
               WHERE s.token_hash = ? AND s.expires_at > ?"#,
            token_hash,
            now,
        )
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| SessionUser {
            id: r.id,
            username: r.username,
        }))
    }

    async fn delete_session(&self, token_hash: &str) -> Result<(), RepoError> {
        sqlx::query!("DELETE FROM sessions WHERE token_hash = ?", token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn delete_expired_sessions(&self, now: DateTime<Utc>) -> Result<(), RepoError> {
        let now = to_timestamp(now);
        sqlx::query!("DELETE FROM sessions WHERE expires_at <= ?", now)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

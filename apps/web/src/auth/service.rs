//! Auth business rules: account creation, login, logout, session lookup.
//! Knows nothing about HTTP or SQL.

use std::sync::Arc;

use anyhow::Context;
use chrono::Duration;
use library_shared::{CurrentUser, LoginRequest};
use rand::TryRng;
use rand::rngs::SysRng;
use sha2::{Digest, Sha256};

use super::password::{DUMMY_HASH, hash_password, verify_password};
use super::repo::AuthRepository;
use crate::clock::Clock;
use crate::db::RepoError;
use crate::error::{AppError, FieldErrors};

const MIN_PASSWORD_CHARS: usize = 8;
const TOKEN_BYTES: usize = 32;

/// What a successful login produces; the route turns it into a cookie.
#[derive(Debug, Clone)]
pub struct LoginSession {
    pub user: CurrentUser,
    /// The raw token. Only ever sent to the client; never stored.
    pub token: String,
}

/// Cheap to clone (two `Arc`s and a number), so it can live in `AppState`.
#[derive(Clone)]
pub struct AuthService {
    repo: Arc<dyn AuthRepository>,
    clock: Arc<dyn Clock>,
    session_ttl: Duration,
}

impl AuthService {
    pub fn new(
        repo: Arc<dyn AuthRepository>,
        clock: Arc<dyn Clock>,
        session_ttl: Duration,
    ) -> Self {
        Self {
            repo,
            clock,
            session_ttl,
        }
    }

    /// How long a session lasts; the route uses it for the cookie's `Max-Age`.
    pub fn session_ttl(&self) -> Duration {
        self.session_ttl
    }

    /// Create a librarian account (used by the `create-user` command).
    pub async fn create_user(
        &self,
        username: &str,
        password: &str,
    ) -> Result<CurrentUser, AppError> {
        let username = username.trim();
        let mut errors = FieldErrors::new();
        if username.is_empty() {
            errors.add("username", "must not be empty");
        }
        if password.chars().count() < MIN_PASSWORD_CHARS {
            errors.add("password", "must be at least 8 characters");
        }
        errors.into_result()?;

        let hash = hash_password(password.to_owned())
            .await
            .map_err(AppError::Internal)?;
        match self
            .repo
            .create_user(username, &hash, self.clock.now())
            .await
        {
            Ok(id) => Ok(CurrentUser {
                id,
                username: username.to_owned(),
            }),
            Err(RepoError::Conflict { .. }) => Err(AppError::Conflict {
                code: "username_taken",
                message: "username already exists".to_owned(),
            }),
            Err(other) => Err(other.into()),
        }
    }

    pub async fn login(&self, request: LoginRequest) -> Result<LoginSession, AppError> {
        let mut errors = FieldErrors::new();
        if request.username.trim().is_empty() {
            errors.add("username", "must not be empty");
        }
        if request.password.is_empty() {
            errors.add("password", "must not be empty");
        }
        errors.into_result()?;

        let user = self
            .repo
            .find_user_by_username(request.username.trim())
            .await?;
        // Always run one verification, even for an unknown user, so both
        // failure paths take about as long.
        let hash = user
            .as_ref()
            .map_or(DUMMY_HASH, |u| u.password_hash.as_str());
        let matches = verify_password(request.password, hash.to_owned())
            .await
            .map_err(AppError::Internal)?;
        let user = match user {
            Some(user) if matches => user,
            _ => return Err(AppError::InvalidCredentials),
        };

        let now = self.clock.now();
        let expires_at = now + self.session_ttl;
        let token = new_token().map_err(AppError::Internal)?;
        self.repo
            .create_session(&hash_token(&token), user.id, now, expires_at)
            .await?;
        // Housekeeping: expired sessions are swept whenever someone logs in.
        // The session already exists, so a failed sweep must not fail the login.
        if let Err(err) = self.repo.delete_expired_sessions(now).await {
            tracing::warn!(error = ?err, "failed to sweep expired sessions");
        }

        Ok(LoginSession {
            user: CurrentUser {
                id: user.id,
                username: user.username,
            },
            token,
        })
    }

    pub async fn logout(&self, token: &str) -> Result<(), AppError> {
        self.repo.delete_session(&hash_token(token)).await?;
        Ok(())
    }

    /// Resolve a raw session token to its user; unknown or expired is `Unauthorized`.
    pub async fn authenticate(&self, token: &str) -> Result<CurrentUser, AppError> {
        self.repo
            .find_session_user(&hash_token(token), self.clock.now())
            .await?
            .map(|user| CurrentUser {
                id: user.id,
                username: user.username,
            })
            .ok_or(AppError::Unauthorized)
    }
}

/// 32 random bytes from the operating system's RNG, hex-encoded (64 chars).
fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|err| anyhow::anyhow!("OS random number generator failed: {err}"))
        .context("generating session token")?;
    Ok(hex::encode(bytes))
}

/// What the database stores instead of the token: its SHA-256, hex-encoded.
/// If the database leaks, the hashes cannot be replayed as cookies. A fast hash
/// is fine here (unlike passwords) because the token is already 256 random bits.
fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};

    use super::*;
    use crate::auth::repo::{SessionUser, UserRecord};
    use crate::clock::FixedClock;

    /// A repository whose session sweep always fails; everything else succeeds.
    struct FailingSweepRepo;

    #[async_trait]
    impl AuthRepository for FailingSweepRepo {
        async fn create_user(&self, _: &str, _: &str, _: DateTime<Utc>) -> Result<i64, RepoError> {
            Ok(1)
        }

        async fn find_user_by_username(
            &self,
            username: &str,
        ) -> Result<Option<UserRecord>, RepoError> {
            Ok(Some(UserRecord {
                id: 1,
                username: username.to_owned(),
                password_hash: hash_password("correct horse".to_owned()).await.unwrap(),
            }))
        }

        async fn create_session(
            &self,
            _: &str,
            _: i64,
            _: DateTime<Utc>,
            _: DateTime<Utc>,
        ) -> Result<(), RepoError> {
            Ok(())
        }

        async fn find_session_user(
            &self,
            _: &str,
            _: DateTime<Utc>,
        ) -> Result<Option<SessionUser>, RepoError> {
            Ok(None)
        }

        async fn delete_session(&self, _: &str) -> Result<(), RepoError> {
            Ok(())
        }

        async fn delete_expired_sessions(&self, _: DateTime<Utc>) -> Result<(), RepoError> {
            Err(RepoError::Unexpected(anyhow::anyhow!("sweep failed")))
        }
    }

    #[tokio::test]
    async fn login_succeeds_even_if_the_expired_session_sweep_fails() {
        let service = AuthService::new(
            Arc::new(FailingSweepRepo),
            Arc::new(FixedClock::new(
                DateTime::from_timestamp(1_800_000_000, 0).unwrap(),
            )),
            Duration::hours(1),
        );
        let session = service
            .login(LoginRequest {
                username: "admin".into(),
                password: "correct horse".into(),
            })
            .await
            .unwrap();
        assert_eq!(session.user.username, "admin");
        assert_eq!(session.token.len(), 64);
    }

    #[test]
    fn token_is_64_hex_chars_and_unique() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn token_hash_is_deterministic_and_differs_from_token() {
        let token = new_token().unwrap();
        assert_eq!(hash_token(&token), hash_token(&token));
        assert_ne!(hash_token(&token), token);
        assert_eq!(hash_token(&token).len(), 64);
    }
}

//! Argon2id password hashing (spec §5.7).
//!
//! Argon2 is deliberately slow and memory-hungry. Running it directly inside
//! an `async fn` would block the executor thread and stall every other request
//! on it, so both functions hand the work to `tokio::task::spawn_blocking`,
//! which runs it on a separate thread pool meant for blocking work.

use anyhow::Context;
use argon2::Argon2;
use argon2::password_hash::{Error as HashError, PasswordHasher, PasswordVerifier};

/// A valid hash (with the default parameters) of a throwaway password.
/// Login verifies against it when the username is unknown, so the response
/// time does not reveal which usernames exist.
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$kULZ+KhiNP3f8wTuk3Haug$bbmpS3ahQNEAWvNyloOuFiyTw9u6U2egWfUVB+aftxE";

/// Hash `password` into a PHC string (`$argon2id$v=19$...`) with a random salt.
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|err| anyhow::anyhow!("hashing password: {err}"))
    })
    .await
    .context("password hashing task failed")?
}

/// `Ok(true)` if `password` matches `hash`, `Ok(false)` if it does not, and
/// `Err` only for a malformed hash or an internal failure.
pub async fn verify_password(password: String, hash: String) -> anyhow::Result<bool> {
    tokio::task::spawn_blocking(move || {
        match Argon2::default().verify_password(password.as_bytes(), hash.as_str()) {
            Ok(()) => Ok(true),
            Err(HashError::PasswordInvalid) => Ok(false),
            Err(err) => Err(anyhow::anyhow!("verifying password: {err}")),
        }
    })
    .await
    .context("password verification task failed")?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hash_and_verify_round_trip() {
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(
            verify_password("correct horse".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!verify_password("wrong".into(), hash).await.unwrap());
    }

    #[tokio::test]
    async fn hashes_are_salted() {
        let a = hash_password("same".into()).await.unwrap();
        let b = hash_password("same".into()).await.unwrap();
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn dummy_hash_parses_and_rejects_other_passwords() {
        let matches = verify_password("anything".into(), DUMMY_HASH.into())
            .await
            .unwrap();
        assert!(!matches);
    }
}

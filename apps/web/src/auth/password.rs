//! Argon2id password hashing (spec §5.7).
//!
//! Argon2 is deliberately slow and memory-hungry. Running it directly inside
//! an `async fn` would block the executor thread and stall every other request
//! on it, so both functions hand the work to `tokio::task::spawn_blocking`,
//! which runs it on a separate thread pool meant for blocking work.
//!
//! Argon2id is memory-hard by design: every hash needs 19 MiB (the default
//! parameters). `spawn_blocking` alone would run as many hashes at once as there
//! are logins in flight (its pool grows to 512 threads), so a login burst could
//! take gigabytes and get the process OOM-killed. A process-wide semaphore lets
//! only a few hashes run at once; further logins wait for a permit instead.
//!
//! The limit is half the cores, not all of them, for CPU as well as memory. Each
//! hash is about 50-100 ms of CPU, and in the load test a step starts with 100
//! new users logging in over 10 s: with one permit per core the web app went
//! from 22% to 60-98% CPU, the 2-core box hit 200%, and every request queued
//! (p99 1,026 ms at 2,500 users, while the average CPU was only 120%). Capped at
//! half the cores, logins queue longer in a burst and other requests keep a core.
//! `PASSWORD_HASH_PERMITS` overrides the limit.

use std::sync::{Arc, OnceLock};

use anyhow::Context;
use argon2::Argon2;
use argon2::password_hash::{Error as HashError, PasswordHasher, PasswordVerifier};
use tokio::sync::Semaphore;

/// How many hashes may run at once: `configured` (`PASSWORD_HASH_PERMITS`, already
/// checked to be positive) if set, otherwise half of `cores`, and at least 1.
fn permits_for(cores: usize, configured: Option<usize>) -> usize {
    configured.unwrap_or_else(|| (cores / 2).max(1))
}

fn default_permits(configured: Option<usize>) -> Arc<Semaphore> {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    Arc::new(Semaphore::new(permits_for(cores, configured)))
}

static HASH_PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();

/// Set the permit count from the config (`PASSWORD_HASH_PERMITS`). Call once at
/// startup, before the first hash; a later call changes nothing. Without it (unit
/// tests, other binaries) the first hash takes the default.
pub fn configure(configured: Option<usize>) {
    let permits = default_permits(configured);
    tracing::info!(
        permits = permits.available_permits(),
        "password hashing limit"
    );
    // Already set means a hash ran first; keep the semaphore it is using.
    let _ = HASH_PERMITS.set(permits);
}

fn hash_permits() -> Arc<Semaphore> {
    HASH_PERMITS.get_or_init(|| default_permits(None)).clone()
}

/// Run `work` on the blocking pool once `permits` has a free permit. The permit is
/// taken *before* `spawn_blocking`, so a waiting login costs only a queued future,
/// not a thread and 19 MiB, and it moves into the blocking task, so it is released
/// when the hash is done, even if the request that wanted it was cancelled (the
/// request timeout does that) and the blocking task keeps running.
async fn run_limited<T: Send + 'static>(
    permits: Arc<Semaphore>,
    work: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    // `acquire_owned` fails only if the semaphore is closed, which nothing here does.
    let permit = permits
        .acquire_owned()
        .await
        .context("password hashing is shut down")?;
    tokio::task::spawn_blocking(move || {
        let result = work();
        drop(permit);
        result
    })
    .await
    .context("password hashing task failed")
}

/// A valid hash (with the default parameters) of a throwaway password.
/// Login verifies against it when the username is unknown, so the response
/// time does not reveal which usernames exist.
pub const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$kULZ+KhiNP3f8wTuk3Haug$bbmpS3ahQNEAWvNyloOuFiyTw9u6U2egWfUVB+aftxE";

/// Hash `password` into a PHC string (`$argon2id$v=19$...`) with a random salt.
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    run_limited(hash_permits(), move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|hash| hash.to_string())
            .map_err(|err| anyhow::anyhow!("hashing password: {err}"))
    })
    .await?
}

/// `Ok(true)` if `password` matches `hash`, `Ok(false)` if it does not, and
/// `Err` only for a malformed hash or an internal failure.
pub async fn verify_password(password: String, hash: String) -> anyhow::Result<bool> {
    run_limited(hash_permits(), move || {
        match Argon2::default().verify_password(password.as_bytes(), hash.as_str()) {
            Ok(()) => Ok(true),
            Err(HashError::PasswordInvalid) => Ok(false),
            Err(err) => Err(anyhow::anyhow!("verifying password: {err}")),
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn default_permits_are_half_the_cores_and_at_least_one() {
        for (cores, expected) in [(1, 1), (2, 1), (3, 1), (4, 2), (16, 8)] {
            assert_eq!(permits_for(cores, None), expected, "{cores} cores");
        }
    }

    #[test]
    fn a_configured_count_overrides_the_default() {
        assert_eq!(permits_for(2, Some(4)), 4);
        assert_eq!(permits_for(16, Some(1)), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn never_more_than_the_permit_count_run_at_once() {
        const PERMITS: usize = 2;
        let permits = Arc::new(Semaphore::new(PERMITS));
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let tasks: Vec<_> = (0..16)
            .map(|_| {
                let (permits, running, peak) = (permits.clone(), running.clone(), peak.clone());
                tokio::spawn(async move {
                    run_limited(permits, move || {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        // Long enough for any extra task to start and be counted.
                        std::thread::sleep(Duration::from_millis(20));
                        running.fetch_sub(1, Ordering::SeqCst);
                    })
                    .await
                    .unwrap();
                })
            })
            .collect();
        for task in tasks {
            task.await.unwrap();
        }

        assert!(peak.load(Ordering::SeqCst) <= PERMITS);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert_eq!(permits.available_permits(), PERMITS);
    }

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

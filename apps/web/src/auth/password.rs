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
//!
//! The 19 MiB block array is pooled, not allocated per hash: one buffer per
//! permit, allocated on first use and never freed. With a fresh allocation per
//! hash, glibc served later blocks from per-thread malloc arenas (the blocking
//! pool's threads come and go) and kept the freed 19 MiB in each: a fresh web
//! container went from 2.8 MB to 270 MB after ~190 logins. With the pool it
//! stays at one buffer plus the app. Hashes are the same PHC strings as the
//! `argon2` crate's own `hash_password`/`verify_password` produce and accept.

use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use anyhow::Context;
use argon2::password_hash::Error as HashError;
use argon2::password_hash::phc::{Output, ParamsString, PasswordHash, Salt};
use argon2::{Algorithm, Argon2, Block, Params, Version};
use tokio::sync::Semaphore;

/// How many hashes may run at once: `configured` (`PASSWORD_HASH_PERMITS`, already
/// checked to be positive) if set, otherwise half of `cores`, and at least 1.
fn permits_for(cores: usize, configured: Option<usize>) -> usize {
    configured.unwrap_or_else(|| (cores / 2).max(1))
}

fn default_permits(configured: Option<usize>) -> usize {
    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    permits_for(cores, configured)
}

/// Argon2 block buffers for the default parameters. A hash holds a permit while it
/// uses one, so at most as many buffers as permits are ever allocated; they go back
/// here afterwards and are never freed.
struct BufferPool {
    free: Mutex<Vec<Vec<Block>>>,
    /// Blocks in one buffer: `Params::default().block_count()`.
    blocks: usize,
}

impl BufferPool {
    fn new() -> Self {
        Self {
            free: Mutex::new(Vec::new()),
            blocks: Params::default().block_count(),
        }
    }

    fn take(&self) -> Vec<Block> {
        // A poisoned lock only means another hash panicked; the list is still valid.
        let reused = self
            .free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        // Argon2 overwrites every block it reads, so reused memory needs no clearing.
        reused.unwrap_or_else(|| vec![Block::default(); self.blocks])
    }

    fn give_back(&self, buffer: Vec<Block>) {
        self.free
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(buffer);
    }

    /// Run `argon` for `password` and `salt`, filling `out`.
    fn hash_into(
        &self,
        argon: &Argon2<'_>,
        password: &[u8],
        salt: &[u8],
        out: &mut [u8],
    ) -> Result<(), HashError> {
        // A stored hash may carry bigger parameters than the pool's. That is rare
        // (only if the defaults were raised and old hashes remain), so it gets a
        // one-off allocation instead of a second pool. Smaller parameters use a
        // prefix of the pooled buffer.
        if argon.params().block_count() > self.blocks {
            let mut one_off = vec![Block::default(); argon.params().block_count()];
            argon.hash_password_into_with_memory(password, salt, out, one_off.as_mut_slice())?;
            return Ok(());
        }
        let mut buffer = self.take();
        let result =
            argon.hash_password_into_with_memory(password, salt, out, buffer.as_mut_slice());
        self.give_back(buffer);
        Ok(result?)
    }
}

/// The permit semaphore and the buffer pool that goes with it (one buffer per permit).
#[derive(Clone)]
struct Hasher {
    permits: Arc<Semaphore>,
    pool: Arc<BufferPool>,
}

impl Hasher {
    fn new(permits: usize) -> Self {
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            pool: Arc::new(BufferPool::new()),
        }
    }

    async fn hash(&self, password: String) -> anyhow::Result<String> {
        let pool = self.pool.clone();
        run_limited(self.permits.clone(), move || {
            hash_with(&pool, password.as_bytes())
        })
        .await?
    }

    async fn verify(&self, password: String, hash: String) -> anyhow::Result<bool> {
        let pool = self.pool.clone();
        run_limited(self.permits.clone(), move || {
            verify_with(&pool, password.as_bytes(), &hash)
                .map_err(|err| anyhow::anyhow!("verifying password: {err}"))
        })
        .await?
    }
}

static HASHER: OnceLock<Hasher> = OnceLock::new();

/// Set the permit count from the config (`PASSWORD_HASH_PERMITS`). Call once at
/// startup, before the first hash; a later call changes nothing. Without it (unit
/// tests, other binaries) the first hash takes the default.
pub fn configure(configured: Option<usize>) {
    let permits = default_permits(configured);
    tracing::info!(permits, "password hashing limit");
    // Already set means a hash ran first; keep the hasher it is using.
    let _ = HASHER.set(Hasher::new(permits));
}

fn hasher() -> Hasher {
    HASHER
        .get_or_init(|| Hasher::new(default_permits(None)))
        .clone()
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

/// Hash with the default parameters, a random 16-byte salt and a 32-byte output,
/// the same as `Argon2::default().hash_password`.
fn hash_with(pool: &BufferPool, password: &[u8]) -> anyhow::Result<String> {
    let argon = Argon2::default();
    let salt = argon2::password_hash::try_generate_salt()
        .map_err(|err| anyhow::anyhow!("generating salt: {err}"))?;
    let mut out = [0u8; Params::DEFAULT_OUTPUT_LEN];
    pool.hash_into(&argon, password, &salt, &mut out)
        .map_err(|err| anyhow::anyhow!("hashing password: {err}"))?;
    let build = || -> Result<PasswordHash, HashError> {
        Ok(PasswordHash {
            algorithm: Algorithm::default().ident(),
            version: Some(Version::default().into()),
            params: ParamsString::try_from(argon.params())?,
            salt: Some(Salt::new(&salt)?),
            hash: Some(Output::new(&out)?),
        })
    };
    let hash = build().map_err(|err| anyhow::anyhow!("hashing password: {err}"))?;
    Ok(hash.to_string())
}

/// Recompute the hash of `password` the way the `argon2` crate's `verify_password`
/// does: algorithm, version, parameters, salt and output length all come from the
/// stored hash `stored`, not from the defaults. `Ok(false)` for a wrong password
/// (or a hash without salt or output), `Err` for a hash that does not parse or
/// names something unsupported.
fn verify_with(pool: &BufferPool, password: &[u8], stored: &str) -> Result<bool, HashError> {
    let parsed = PasswordHash::new(stored)?;
    let (Some(salt), Some(expected)) = (&parsed.salt, &parsed.hash) else {
        return Ok(false);
    };
    let algorithm = Algorithm::try_from(parsed.algorithm.as_str())?;
    let version = parsed
        .version
        .map(Version::try_from)
        .transpose()?
        .unwrap_or_default();
    let argon = Argon2::new(algorithm, version, Params::try_from(&parsed)?);

    let mut buffer = [0u8; Output::MAX_LENGTH];
    let out = buffer
        .get_mut(..expected.len())
        .ok_or(HashError::OutputSize)?;
    pool.hash_into(&argon, password, salt, out)?;
    // `Output`'s `==` is constant-time.
    Ok(Output::new(out)? == *expected)
}

/// Hash `password` into a PHC string (`$argon2id$v=19$...`) with a random salt.
pub async fn hash_password(password: String) -> anyhow::Result<String> {
    hasher().hash(password).await
}

/// `Ok(true)` if `password` matches `hash`, `Ok(false)` if it does not, and
/// `Err` only for a malformed hash or an internal failure.
pub async fn verify_password(password: String, hash: String) -> anyhow::Result<bool> {
    hasher().verify(password, hash).await
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

    /// Made by the `argon2` crate's own `hash_password`, before the pool existed.
    const OLD_PATH_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$yBFH+lRgivcsLeNKzi7kOw$IWZt9XVxRTuBvmtqVD09+bnkjj1kZqBSvZnlFTNqZl8";

    #[tokio::test]
    async fn a_hash_from_the_old_code_path_verifies() {
        assert!(
            verify_password("old path password".into(), OLD_PATH_HASH.into())
                .await
                .unwrap()
        );
        assert!(
            !verify_password("old path passwore".into(), OLD_PATH_HASH.into())
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_new_hash_verifies_with_the_argon2_crate() {
        use argon2::PasswordVerifier;

        let hash = hash_password("new path password".into()).await.unwrap();
        // Same PHC format as before: id, version, parameters, 16-byte salt (22
        // base64 characters), 32-byte output (43 characters).
        let parts: Vec<_> = hash.split('$').collect();
        assert_eq!(parts[..4], ["", "argon2id", "v=19", "m=19456,t=2,p=1"]);
        assert_eq!((parts[4].len(), parts[5].len()), (22, 43));
        assert!(
            Argon2::default()
                .verify_password(b"new path password", hash.as_str())
                .is_ok()
        );
        assert!(
            Argon2::default()
                .verify_password(b"other", hash.as_str())
                .is_err()
        );
    }

    #[tokio::test]
    async fn parameters_and_output_length_come_from_the_stored_hash() {
        use argon2::{ParamsBuilder, PasswordHasher};

        let hasher = Hasher::new(1);
        // Smaller than the pooled buffer, with a 16-byte output.
        let small = Params::new(64, 1, 1, Some(16)).unwrap();
        let hash = Argon2::from(small)
            .hash_password(b"small params")
            .unwrap()
            .to_string();
        assert!(
            hasher
                .verify("small params".into(), hash.clone())
                .await
                .unwrap()
        );
        assert!(!hasher.verify("small param".into(), hash).await.unwrap());

        // Bigger than the pooled buffer: a one-off allocation, the pool stays at its
        // one buffer from the small hashes.
        let big_blocks = u32::try_from(hasher.pool.blocks).unwrap() + 64;
        let big = ParamsBuilder::new()
            .m_cost(big_blocks)
            .t_cost(1)
            .build()
            .unwrap();
        let hash = Argon2::from(big)
            .hash_password(b"big params")
            .unwrap()
            .to_string();
        assert!(hasher.verify("big params".into(), hash).await.unwrap());
        assert_eq!(hasher.pool.free.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn malformed_and_unsalted_hashes() {
        let hasher = Hasher::new(1);
        assert!(
            hasher
                .verify("x".into(), "not a hash".into())
                .await
                .is_err()
        );
        assert!(
            !hasher
                .verify("x".into(), "$argon2id$v=19$m=19456,t=2,p=1".into())
                .await
                .unwrap()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_verifications_share_one_permit_and_one_buffer() {
        let hasher = Hasher::new(1);
        let (a, b) = tokio::join!(
            hasher.verify("old path password".into(), OLD_PATH_HASH.into()),
            hasher.verify("old path password".into(), OLD_PATH_HASH.into()),
        );
        assert!(a.unwrap() && b.unwrap());
        assert_eq!(hasher.pool.free.lock().unwrap().len(), 1);
        assert_eq!(hasher.permits.available_permits(), 1);
    }
}

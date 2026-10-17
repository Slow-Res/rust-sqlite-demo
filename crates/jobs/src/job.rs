//! What a job is (spec §5): a serde payload with a versioned kind, a queue,
//! options, and an async `perform`.

use std::fmt;
use std::future::Future;
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::backoff::Backoff;
use crate::context::JobContext;

/// A background job. `KIND` carries a version suffix (`loan_email.v1`): change
/// the payload, change the version, so old and new workers can coexist during
/// a deploy (spec §4.1, §19).
///
/// Delivery is at least once: `perform` may run again after a crash, so it
/// must be idempotent (use [`JobContext::idempotency_key`] towards remotes).
pub trait Job: Serialize + DeserializeOwned + Send + Sync + 'static {
    const KIND: &'static str;
    const QUEUE: &'static str;

    fn options() -> JobOptions {
        JobOptions::default()
    }

    /// Jobs with the same kind and key are de-duplicated while waiting or running (spec §6.3).
    fn unique_key(&self) -> Option<String> {
        None
    }

    /// At most one running job per key, across all processes (spec §12.2).
    fn concurrency_key(&self) -> Option<String> {
        None
    }

    fn perform(self, ctx: JobContext) -> impl Future<Output = Result<(), JobError>> + Send;
}

/// Most attempts a kind may allow: far beyond any useful retry schedule, and small
/// enough that retry-delay arithmetic cannot overflow.
pub const MAX_ATTEMPTS: i32 = 1000;

/// Per-kind defaults, stored on each job row at enqueue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobOptions {
    pub max_attempts: i32,
    /// Lease expiries tolerated before the job fails (spec §9.3).
    pub max_crashes: i32,
    /// Longest a single attempt may run (spec §8.4).
    pub deadline: Duration,
    /// Lease length; must be at least 3 heartbeats (validated by the registry).
    pub lease: Duration,
    pub priority: i16,
    pub backoff: Backoff,
}

impl Default for JobOptions {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            max_crashes: 2,
            deadline: Duration::from_secs(15 * 60),
            lease: Duration::from_secs(60),
            priority: 0,
            backoff: Backoff::default(),
        }
    }
}

impl JobOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_attempts < 1 {
            return Err("max_attempts must be at least 1".into());
        }
        if self.max_attempts > MAX_ATTEMPTS {
            return Err(format!("max_attempts must be at most {MAX_ATTEMPTS}"));
        }
        if self.max_crashes < 1 {
            return Err("max_crashes must be at least 1".into());
        }
        if self.deadline < Duration::from_secs(1) || self.lease < Duration::from_secs(1) {
            return Err("deadline and lease must be at least 1 s".into());
        }
        if self.deadline.as_secs() > i32::MAX as u64 || self.lease.as_secs() > i32::MAX as u64 {
            return Err("deadline and lease must fit in i32 seconds".into());
        }
        Ok(())
    }

    /// Whole seconds, rounded up, as stored in `jobs.deadline_s`.
    pub fn deadline_s(&self) -> i32 {
        secs_ceil(self.deadline)
    }

    pub fn lease_s(&self) -> i32 {
        secs_ceil(self.lease)
    }
}

pub(crate) fn secs_ceil(d: Duration) -> i32 {
    let s = d.as_secs() + u64::from(d.subsec_nanos() > 0);
    i32::try_from(s.max(1)).unwrap_or(i32::MAX)
}

/// How an attempt failed (spec §5, §9.1).
#[derive(Debug)]
pub enum JobError {
    /// Try again after the kind's backoff.
    Retry(String),
    /// Try again, but not before this delay (e.g. HTTP 429 `Retry-After`).
    RetryAfter(Duration, String),
    /// Never retry: bad input, a 4xx answer, a missing dependency.
    Permanent(String),
    /// The handler stopped because `ctx.cancelled()` fired.
    Cancelled,
}

impl JobError {
    pub fn retry(err: impl fmt::Display) -> Self {
        Self::Retry(err.to_string())
    }

    pub fn permanent(err: impl fmt::Display) -> Self {
        Self::Permanent(err.to_string())
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Retry(m) => write!(f, "retryable: {m}"),
            Self::RetryAfter(d, m) => write!(f, "retry after {d:?}: {m}"),
            Self::Permanent(m) => write!(f, "permanent: {m}"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// `?` on an `anyhow` error means "retry".
impl From<anyhow::Error> for JobError {
    fn from(err: anyhow::Error) -> Self {
        Self::Retry(format!("{err:#}"))
    }
}

impl From<sqlx::Error> for JobError {
    fn from(err: sqlx::Error) -> Self {
        Self::Retry(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_attempts_is_capped() {
        let with = |max_attempts| JobOptions {
            max_attempts,
            ..JobOptions::default()
        };
        assert!(with(1).validate().is_ok());
        assert!(with(MAX_ATTEMPTS).validate().is_ok());
        assert!(with(MAX_ATTEMPTS + 1).validate().is_err());
        assert!(with(i32::MAX).validate().is_err());
        assert!(with(0).validate().is_err());
    }
}

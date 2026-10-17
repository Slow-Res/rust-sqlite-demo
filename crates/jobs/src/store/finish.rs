//! Outcome writes, all fenced by the claim's identity (spec §8.3, §9.1).
//! A write that updates 0 rows means the lease was lost (reaped, maybe
//! re-claimed elsewhere): the caller drops its result.

use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;

use crate::backoff::Backoff;

/// Longest error message stored in `errors[].message`, in characters.
pub const MAX_ERROR_CHARS: usize = 2000;

/// Longest retry delay we will schedule. A remote's absurd `Retry-After` must
/// not overflow Postgres' timestamp range: that would make the write fail and
/// leave the job running until the reaper counts it as a crash.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(7 * 24 * 3600);

/// Who holds the job, for which attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fence {
    pub job_id: i64,
    pub process_id: i64,
    pub attempt: i32,
}

/// Why an attempt ended without success, as stored in `errors[].cause`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    Error,
    Panic,
    DeadlineExceeded,
    Permanent,
    BadArgs,
    Cancelled,
    ReleasedOnShutdown,
}

impl Cause {
    /// The value stored in `errors[].cause`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Panic => "panic",
            Self::DeadlineExceeded => "deadline_exceeded",
            Self::Permanent => "permanent",
            Self::BadArgs => "bad_args",
            Self::Cancelled => "cancelled",
            Self::ReleasedOnShutdown => "released_on_shutdown",
        }
    }
}

/// What happens to a job after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    RetryIn(Duration),
    Fail,
}

/// Retry while attempts remain and the error allows it; the delay is the
/// kind's backoff, but never shorter than `min_delay` (a `Retry-After`) and
/// never longer than [`MAX_RETRY_DELAY`].
pub fn next_after_failure(
    attempt: i32,
    max_attempts: i32,
    retryable: bool,
    min_delay: Option<Duration>,
    backoff: &Backoff,
) -> Next {
    if !retryable || attempt >= max_attempts {
        return Next::Fail;
    }
    let delay = backoff.delay(attempt).max(min_delay.unwrap_or_default());
    Next::RetryIn(delay.min(MAX_RETRY_DELAY))
}

/// Cap the length, and replace NUL, which Postgres text and jsonb reject (the
/// write would fail and the job would stay running until reaped).
pub(crate) fn truncate(message: &str) -> String {
    message
        .chars()
        .take(MAX_ERROR_CHARS)
        .map(|c| if c == '\0' { '\u{FFFD}' } else { c })
        .collect()
}

/// Mark the job `succeeded`. `false` means the fence rejected the write.
pub async fn succeed(pool: &PgPool, f: Fence) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'succeeded', finished_at = now(), locked_by = NULL, locked_until = NULL
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Where [`fail_attempt`] left the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Landed {
    /// Back to `available`, to run again at the scheduled time.
    Retry,
    /// Out of attempts or not retryable: terminal.
    Failed,
    /// A retry was due but `cancel_requested` was set: terminal, not requeued.
    Cancelled,
}

/// Record a failed attempt, then either schedule a retry or fail the job for
/// good. A retry leaves `finished_at` NULL (`jobs_finished_iff_terminal`); a
/// retry of a job whose cancellation was requested ends `cancelled` instead,
/// so the request is not lost when the handler failed before it noticed.
/// `None` means the fence rejected the write.
pub async fn fail_attempt(
    pool: &PgPool,
    f: Fence,
    cause: Cause,
    message: &str,
    next: Next,
) -> Result<Option<Landed>, sqlx::Error> {
    let (retry, delay_s) = match next {
        Next::RetryIn(d) => (true, d.as_secs_f64()),
        Next::Fail => (false, 0.0),
    };
    let state: Option<String> = sqlx::query_scalar(
        "UPDATE jobs SET
             state = CASE WHEN NOT $4 THEN 'failed' WHEN cancel_requested THEN 'cancelled' ELSE 'available' END,
             run_at = CASE WHEN $4 AND NOT cancel_requested THEN now() + make_interval(secs => $5::float8) ELSE run_at END,
             finished_at = CASE WHEN $4 AND NOT cancel_requested THEN NULL ELSE now() END,
             locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', $6::text, 'message', $7::text))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3
         RETURNING state",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(retry)
    .bind(delay_s)
    .bind(cause.as_str())
    .bind(truncate(message))
    .fetch_optional(pool)
    .await?;
    Ok(state.map(|s| match s.as_str() {
        "available" => Landed::Retry,
        "cancelled" => Landed::Cancelled,
        _ => Landed::Failed,
    }))
}

/// Mark the job `cancelled` (terminal), recording `message` as the reason.
pub async fn cancel(pool: &PgPool, f: Fence, message: &str) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'cancelled', finished_at = now(), locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', 'cancelled', 'message', $4::text))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(truncate(message))
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Shutdown is not the job's fault: give the attempt back (spec §13 step 4).
pub async fn release(pool: &PgPool, f: Fence) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET state = 'available', attempt = attempt - 1, run_at = now(),
             locked_by = NULL, locked_until = NULL,
             errors = jobs_push_error(errors, jsonb_build_object(
                 'attempt', attempt, 'at', now(), 'cause', 'released_on_shutdown',
                 'message', 'the worker shut down before the job finished'))
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

/// Store the handler's progress report; fenced so a stale worker cannot touch it.
pub async fn write_progress(
    pool: &PgPool,
    f: Fence,
    progress: &Value,
) -> Result<bool, sqlx::Error> {
    let r = sqlx::query(
        "UPDATE jobs SET progress = $4
         WHERE id = $1 AND state = 'running' AND locked_by = $2 AND attempt = $3",
    )
    .bind(f.job_id)
    .bind(f.process_id)
    .bind(f.attempt)
    .bind(progress)
    .execute(pool)
    .await?;
    Ok(r.rows_affected() == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_until_the_last_attempt_honouring_min_delay() {
        let b = Backoff {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(1),
        };
        assert!(
            matches!(next_after_failure(1, 3, true, None, &b), Next::RetryIn(d) if d <= Duration::from_secs(1))
        );
        assert!(matches!(
            next_after_failure(3, 3, true, None, &b),
            Next::Fail
        ));
        assert!(matches!(
            next_after_failure(1, 3, false, None, &b),
            Next::Fail
        ));
        assert!(matches!(
            next_after_failure(1, 3, true, Some(Duration::from_secs(120)), &b),
            Next::RetryIn(d) if d == Duration::from_secs(120)
        ));
    }

    #[test]
    fn retry_delay_is_capped() {
        let b = Backoff {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(1),
        };
        assert_eq!(
            next_after_failure(1, 3, true, Some(Duration::MAX), &b),
            Next::RetryIn(MAX_RETRY_DELAY)
        );
    }
}

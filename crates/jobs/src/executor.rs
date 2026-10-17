//! Running one claimed job and recording its outcome (spec §8.4, §9.1).

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::context::JobContext;
use crate::job::JobError;
use crate::metrics::Metrics;
use crate::rate_limit::RateLimiter;
use crate::registry::Registry;
use crate::store::claim::ClaimedJob;
use crate::store::finish::{self, Cause, Fence, Landed, Next, next_after_failure};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum CancelReason {
    /// An operator or the app asked (`cancel_requested`).
    Requested = 1,
    /// The heartbeat could not renew the lease; another worker may run the job.
    LeaseLost = 2,
    /// This process is shutting down.
    Shutdown = 3,
}

/// Why a handler's token fired. The first reason set wins.
#[derive(Debug, Default)]
pub struct CancelCell {
    reason: AtomicU8,
    /// Set the first time the lease of this attempt is reported lost, whatever
    /// reason won, so `jobs_lease_lost_total` counts each lost lease once.
    lost: AtomicBool,
}

impl CancelCell {
    pub fn set(&self, reason: CancelReason) {
        let _ = self
            .reason
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Record that the lease was lost; true only for the first report.
    pub fn mark_lease_lost(&self) -> bool {
        !self.lost.swap(true, Ordering::AcqRel)
    }

    pub fn get(&self) -> Option<CancelReason> {
        match self.reason.load(Ordering::Acquire) {
            1 => Some(CancelReason::Requested),
            2 => Some(CancelReason::LeaseLost),
            3 => Some(CancelReason::Shutdown),
            _ => None,
        }
    }
}

pub struct Execution {
    pub job: ClaimedJob,
    pub process_id: i64,
    pub token: CancellationToken,
    pub cancel: Arc<CancelCell>,
    pub progress: Arc<Mutex<Option<Value>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    Succeeded,
    Retrying,
    Failed,
    Cancelled,
    Released,
    LeaseLost,
    /// The outcome write changed nothing: the lease was lost meanwhile.
    FenceRejected,
}

impl Finished {
    /// Every outcome, for pre-registering metric series.
    pub const ALL: [Finished; 7] = [
        Self::Succeeded,
        Self::Retrying,
        Self::Failed,
        Self::Cancelled,
        Self::Released,
        Self::LeaseLost,
        Self::FenceRejected,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Retrying => "retrying",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Released => "released",
            Self::LeaseLost => "lease_lost",
            Self::FenceRejected => "fence_rejected",
        }
    }
}

/// What the handler did, before deciding what to write.
enum Ran {
    Ok,
    Err(JobError),
    Panicked(String),
    Deadline,
    /// The arguments no longer decode: permanent, never retried.
    BadArgs(String),
}

pub async fn execute(
    pool: &PgPool,
    registry: &Registry,
    limiter: Arc<RateLimiter>,
    metrics: &Metrics,
    exec: Execution,
) -> Finished {
    let Execution {
        job,
        process_id,
        token,
        cancel,
        progress,
    } = exec;
    let fence = Fence {
        job_id: job.id,
        process_id,
        attempt: job.attempt,
    };
    let Some(entry) = registry.get(&job.kind) else {
        // Claims filter by known kinds, so this means a registry bug; never retry it here.
        let msg = "unknown kind";
        let finished = write(fence, metrics, || {
            attempt_failed(pool, fence, Cause::Permanent, msg, Next::Fail)
        })
        .await;
        log_outcome(&job, finished, Some((Cause::Permanent, msg, None)));
        return finished;
    };
    let ctx = JobContext::new(
        pool.clone(),
        job.id,
        job.attempt,
        Arc::from(job.kind.as_str()),
        token.clone(),
        progress,
        limiter,
        registry.deps(),
    );
    let deadline = Duration::from_secs(u64::try_from(job.deadline_s).unwrap_or(1));
    // Own task: a panic becomes a JoinError instead of unwinding through the worker.
    // The handler constructor runs inside it too: it decodes the arguments (a custom
    // `Deserialize`) and runs the synchronous part of `perform`, both user code.
    let make = entry.handler.clone();
    let args = job.args.clone();
    let mut handle = tokio::spawn(async move {
        match make(args, ctx) {
            Ok(future) => Ok(future.await),
            Err(decode) => Err(decode),
        }
    });
    let ran = tokio::select! {
        joined = &mut handle => match joined {
            Ok(Ok(Ok(()))) => Ran::Ok,
            Ok(Ok(Err(e))) => Ran::Err(e),
            Ok(Err(decode)) => Ran::BadArgs(decode.to_string()),
            Err(e) if e.is_panic() => Ran::Panicked(panic_message(e)),
            Err(e) => Ran::Err(JobError::retry(e)),
        },
        () = tokio::time::sleep(deadline) => {
            handle.abort();
            Ran::Deadline
        }
    };
    let backoff = entry.options.backoff;
    let retry = |cause: Cause, msg: String, min: Option<Duration>| {
        let next = next_after_failure(job.attempt, job.max_attempts, true, min, &backoff);
        (cause, msg, next)
    };

    // A fired token decides first: the handler stopped because we asked it to.
    // (A decode failure is not caused by the cancellation, so it is recorded regardless.)
    if token.is_cancelled() && !matches!(ran, Ran::Ok | Ran::BadArgs(_)) {
        match cancel.get() {
            Some(CancelReason::Shutdown) => {
                let finished = write(fence, metrics, || {
                    flagged(finish::release(pool, fence), Finished::Released)
                })
                .await;
                log_outcome(&job, finished, None);
                return finished;
            }
            // The heartbeat stops renewing a job at its deadline (in database time), which
            // can land a few ms before this task's own timer fires. A handler that hit its
            // deadline is recorded as such; if the lease was really lost, the fence below
            // rejects the write and nothing changes.
            Some(CancelReason::LeaseLost) if !matches!(ran, Ran::Deadline) => {
                return Finished::LeaseLost;
            }
            Some(CancelReason::LeaseLost | CancelReason::Requested) | None => {}
        }
    }
    let (cause, msg, next) = match ran {
        Ran::Ok => {
            let finished = write(fence, metrics, || {
                flagged(finish::succeed(pool, fence), Finished::Succeeded)
            })
            .await;
            log_outcome(&job, finished, None);
            return finished;
        }
        Ran::Err(JobError::Cancelled) => {
            let finished = write(fence, metrics, || {
                flagged(
                    finish::cancel(pool, fence, "cancelled"),
                    Finished::Cancelled,
                )
            })
            .await;
            log_outcome(&job, finished, None);
            return finished;
        }
        Ran::Err(JobError::Permanent(m)) => (Cause::Permanent, m, Next::Fail),
        Ran::Err(JobError::Retry(m)) => retry(Cause::Error, m, None),
        Ran::Err(JobError::RetryAfter(d, m)) => retry(Cause::Error, m, Some(d)),
        Ran::BadArgs(err) => {
            // The serde message can quote argument values: it goes to the database
            // error only, never to the logs.
            tracing::error!(job_id = job.id, kind = %job.kind, "jobs.bad_args");
            let msg = format!("arguments do not decode as {}: {err}", job.kind);
            (Cause::BadArgs, msg, Next::Fail)
        }
        Ran::Panicked(m) => retry(Cause::Panic, m, None),
        Ran::Deadline => retry(
            Cause::DeadlineExceeded,
            format!("did not finish within {deadline:?}"),
            None,
        ),
    };
    let finished = write(fence, metrics, || {
        attempt_failed(pool, fence, cause, &msg, next)
    })
    .await;
    let delay = match next {
        Next::RetryIn(d) => Some(d),
        Next::Fail => None,
    };
    log_outcome(&job, finished, Some((cause, &msg, delay)));
    finished
}

/// [`finish::fail_attempt`] as a [`Finished`]: where the job landed decides the outcome.
async fn attempt_failed(
    pool: &PgPool,
    fence: Fence,
    cause: Cause,
    msg: &str,
    next: Next,
) -> Result<Option<Finished>, sqlx::Error> {
    let landed = finish::fail_attempt(pool, fence, cause, msg, next).await?;
    Ok(landed.map(|l| match l {
        Landed::Retry => Finished::Retrying,
        Landed::Failed => Finished::Failed,
        Landed::Cancelled => Finished::Cancelled,
    }))
}

/// Turn a fenced write's "did a row change" into the outcome it stands for.
async fn flagged(
    write: impl std::future::Future<Output = Result<bool, sqlx::Error>>,
    finished: Finished,
) -> Result<Option<Finished>, sqlx::Error> {
    Ok(write.await?.then_some(finished))
}

/// The spec §17.1 events for an outcome that was written. Never the arguments.
/// `failure` is the cause, the message (cut like the stored one) and the retry delay.
fn log_outcome(
    job: &ClaimedJob,
    finished: Finished,
    failure: Option<(Cause, &str, Option<Duration>)>,
) {
    let (job_id, kind, queue, attempt) =
        (job.id, job.kind.as_str(), job.queue.as_str(), job.attempt);
    // Undecodable arguments: the message can quote argument values, so it stays in
    // the database error (see `jobs.bad_args`).
    let failure = failure.map(|(cause, message, delay)| {
        let message = if cause == Cause::BadArgs { "" } else { message };
        (cause, message, delay)
    });
    match (finished, failure) {
        (Finished::Failed, Some((cause, message, _))) => tracing::warn!(
            job_id, %kind, %queue, attempt,
            cause = %cause.as_str(),
            error = %finish::truncate(message),
            "jobs.failed"
        ),
        (Finished::Retrying, Some((cause, message, delay))) => tracing::info!(
            job_id, %kind, %queue, attempt,
            cause = %cause.as_str(),
            error = %finish::truncate(message),
            delay_s = delay.map_or(0.0, |d| d.as_secs_f64()),
            "jobs.retry_scheduled"
        ),
        (Finished::Cancelled, _) => {
            tracing::info!(job_id, %kind, %queue, attempt, "jobs.cancelled")
        }
        (Finished::Released, _) => tracing::info!(job_id, %kind, %queue, attempt, "jobs.released"),
        (Finished::Succeeded, _) => {
            tracing::debug!(job_id, %kind, %queue, attempt, "jobs.succeeded")
        }
        _ => {}
    }
}

/// Run a fenced write, retrying database errors (spec §11.5) until it is
/// accepted (→ the outcome it reports) or rejected by the fence (→ `FenceRejected`).
async fn write<F, Fut>(fence: Fence, metrics: &Metrics, op: F) -> Finished
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Option<Finished>, sqlx::Error>>,
{
    let mut wait = Duration::from_secs(1);
    loop {
        match op().await {
            Ok(Some(finished)) => return finished,
            Ok(None) => {
                tracing::warn!(
                    job_id = fence.job_id,
                    attempt = fence.attempt,
                    "jobs.fence_rejected"
                );
                return Finished::FenceRejected;
            }
            Err(err) => {
                metrics.db_error("finish");
                tracing::warn!(job_id = fence.job_id, error = %err, "jobs: outcome write failed; retrying");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(30));
            }
        }
    }
}

fn panic_message(err: tokio::task::JoinError) -> String {
    match err.try_into_panic() {
        Ok(payload) => payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_owned())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic with a non-string payload".to_owned()),
        Err(_) => "task failed".to_owned(),
    }
}

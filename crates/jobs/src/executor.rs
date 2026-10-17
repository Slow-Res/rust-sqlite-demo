//! Running one claimed job and recording its outcome (spec §8.4, §9.1).

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::context::JobContext;
use crate::job::JobError;
use crate::rate_limit::RateLimiter;
use crate::registry::Registry;
use crate::store::claim::ClaimedJob;
use crate::store::finish::{self, Cause, Fence, Next, next_after_failure};

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
pub struct CancelCell(AtomicU8);

impl CancelCell {
    pub fn set(&self, reason: CancelReason) {
        let _ = self
            .0
            .compare_exchange(0, reason as u8, Ordering::AcqRel, Ordering::Acquire);
    }

    pub fn get(&self) -> Option<CancelReason> {
        match self.0.load(Ordering::Acquire) {
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
}

pub async fn execute(
    pool: &PgPool,
    registry: &Registry,
    limiter: Arc<RateLimiter>,
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
        return write(
            fence,
            || finish::fail_attempt(pool, fence, Cause::Permanent, "unknown kind", Next::Fail),
            Finished::Failed,
        )
        .await;
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
    let future = match (entry.handler)(job.args.clone(), ctx) {
        Ok(f) => f,
        Err(err) => {
            let msg = format!("arguments do not decode as {}: {err}", job.kind);
            tracing::error!(job_id = job.id, kind = %job.kind, error = %msg, "jobs.bad_args");
            return write(
                fence,
                || finish::fail_attempt(pool, fence, Cause::BadArgs, &msg, Next::Fail),
                Finished::Failed,
            )
            .await;
        }
    };
    let deadline = Duration::from_secs(u64::try_from(job.deadline_s).unwrap_or(1));
    // Own task: a panic becomes a JoinError instead of unwinding through the worker.
    let mut handle = tokio::spawn(future);
    let ran = tokio::select! {
        joined = &mut handle => match joined {
            Ok(Ok(())) => Ran::Ok,
            Ok(Err(e)) => Ran::Err(e),
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
        let finished = if matches!(next, Next::Fail) {
            Finished::Failed
        } else {
            Finished::Retrying
        };
        (cause, msg, next, finished)
    };

    // A fired token decides first: the handler stopped because we asked it to.
    if token.is_cancelled() && !matches!(ran, Ran::Ok) {
        match cancel.get() {
            Some(CancelReason::Shutdown) => {
                return write(fence, || finish::release(pool, fence), Finished::Released).await;
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
    let (cause, msg, next, finished) = match ran {
        Ran::Ok => return write(fence, || finish::succeed(pool, fence), Finished::Succeeded).await,
        Ran::Err(JobError::Cancelled) => {
            return write(
                fence,
                || finish::cancel(pool, fence, "cancelled"),
                Finished::Cancelled,
            )
            .await;
        }
        Ran::Err(JobError::Permanent(m)) => (Cause::Permanent, m, Next::Fail, Finished::Failed),
        Ran::Err(JobError::Retry(m)) => retry(Cause::Error, m, None),
        Ran::Err(JobError::RetryAfter(d, m)) => retry(Cause::Error, m, Some(d)),
        Ran::Panicked(m) => retry(Cause::Panic, m, None),
        Ran::Deadline => retry(
            Cause::DeadlineExceeded,
            format!("did not finish within {deadline:?}"),
            None,
        ),
    };
    write(
        fence,
        || finish::fail_attempt(pool, fence, cause, &msg, next),
        finished,
    )
    .await
}

/// Run a fenced write, retrying database errors (spec §11.5) until it is
/// accepted (→ `finished`) or rejected by the fence (→ `FenceRejected`).
async fn write<F, Fut>(fence: Fence, op: F, finished: Finished) -> Finished
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<bool, sqlx::Error>>,
{
    let mut wait = Duration::from_secs(1);
    loop {
        match op().await {
            Ok(true) => return finished,
            Ok(false) => {
                tracing::warn!(
                    job_id = fence.job_id,
                    attempt = fence.attempt,
                    "jobs.fence_rejected"
                );
                return Finished::FenceRejected;
            }
            Err(err) => {
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

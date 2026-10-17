//! The heartbeat task (spec §8.2): its own task and connection, never a
//! handler's, so a busy handler cannot starve it. Also flushes progress (§10).

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

use crate::executor::CancelReason;
use crate::running::Running;
use crate::store::finish::write_progress;
use crate::store::process::{Beat, heartbeat};

/// Liveness for `/readyz`.
#[derive(Debug, Default)]
pub struct Health {
    last_beat_ms: AtomicU64,
    fenced: AtomicBool,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl Health {
    pub fn beat_ok(&self) {
        self.last_beat_ms.store(now_ms(), Ordering::Release);
    }

    /// Time since the last successful heartbeat (process clock: only compared with itself).
    pub fn last_beat_age(&self) -> Option<Duration> {
        match self.last_beat_ms.load(Ordering::Acquire) {
            0 => None,
            ms => Some(Duration::from_millis(now_ms().saturating_sub(ms))),
        }
    }

    pub fn set_fenced(&self) {
        self.fenced.store(true, Ordering::Release);
    }

    pub fn is_fenced(&self) -> bool {
        self.fenced.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatExit {
    Stopped,
    /// The registry row was deleted: the process must stop (spec §8.2).
    SelfFenced,
}

/// Beat every `every` until `stop`. When beats fail for longer than
/// `give_up_after` (the shortest lease), every handler is cancelled, because
/// another worker may legitimately take its job over.
pub async fn run(
    pool: PgPool,
    process_id: i64,
    running: Running,
    every: Duration,
    give_up_after: Duration,
    health: Arc<Health>,
    stop: CancellationToken,
) -> HeartbeatExit {
    let mut beat = tokio::time::interval(every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut flush = tokio::time::interval(Duration::from_secs(1));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_ok = tokio::time::Instant::now();
    loop {
        tokio::select! {
            () = stop.cancelled() => return HeartbeatExit::Stopped,
            _ = flush.tick() => {
                for (fence, value) in running.take_progress(process_id) {
                    if let Err(err) = write_progress(&pool, fence, &value).await {
                        tracing::warn!(job_id = fence.job_id, error = %err, "jobs: progress write failed");
                    }
                }
            }
            _ = beat.tick() => {
                let ids = running.ids();
                match heartbeat(&pool, process_id, &ids).await {
                    Ok(Beat::Pruned) => {
                        tracing::error!(process_id, "process.self_fenced: registry row gone; stopping");
                        health.set_fenced();
                        running.cancel_all(CancelReason::LeaseLost);
                        return HeartbeatExit::SelfFenced;
                    }
                    Ok(Beat::Alive(rows)) => {
                        last_ok = tokio::time::Instant::now();
                        health.beat_ok();
                        let renewed: HashSet<i64> = rows.iter().map(|r| r.id).collect();
                        for id in ids.iter().filter(|id| !renewed.contains(id)) {
                            tracing::warn!(job_id = id, "jobs.lease_lost");
                            running.cancel(*id, CancelReason::LeaseLost);
                        }
                        for row in rows.iter().filter(|r| r.cancel_requested) {
                            running.cancel(row.id, CancelReason::Requested);
                        }
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "jobs: heartbeat failed");
                        if last_ok.elapsed() > give_up_after && !running.is_empty() {
                            tracing::error!("jobs: no heartbeat for a whole lease; cancelling all handlers");
                            running.cancel_all(CancelReason::LeaseLost);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_tracks_beats_and_fencing() {
        let h = Health::default();
        assert!(h.last_beat_age().is_none());
        h.beat_ok();
        assert!(
            h.last_beat_age()
                .is_some_and(|a| a < Duration::from_secs(1))
        );
        assert!(!h.is_fenced());
        h.set_fenced();
        assert!(h.is_fenced());
    }
}

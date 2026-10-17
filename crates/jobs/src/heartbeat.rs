//! The heartbeat task (spec §8.2): its own task and connection, never a
//! handler's, so a busy handler cannot starve it. Also flushes progress (§10).

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use sqlx::PgPool;
use tokio::time::{Instant, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::executor::CancelReason;
use crate::metrics::Metrics;
use crate::running::Running;
use crate::store::finish::{Fence, write_progress};
use crate::store::process::{Beat, LeaseRow, heartbeat};

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

/// How often pending progress values are written out.
const FLUSH_EVERY: Duration = Duration::from_secs(1);
/// Longest a single progress write may take before it counts as failed.
const FLUSH_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Beat every `every` until `stop`.
///
/// `shortest_lease` is the shortest lease of any job kind this process runs
/// (`Registry::validate` guarantees it is at least three beats). Every handler
/// is cancelled with [`CancelReason::LeaseLost`] at `last_ok + (shortest_lease -
/// every)`, where `last_ok` is when the last successful beat *started*: that is
/// one beat before the lease can expire, so a handler never outlives its lease
/// while the reaper re-claims the job elsewhere. That deadline is raced against
/// the in-flight beat, so it fires on time even when the database connection
/// stalls without an error. During an outage the task cancels, logs and keeps
/// beating (spec §11.5); only a deleted registry row ([`Beat::Pruned`]) fences.
///
/// Progress is flushed by a separate task, so a slow progress write cannot
/// delay a beat.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    pool: PgPool,
    process_id: i64,
    running: Running,
    every: Duration,
    shortest_lease: Duration,
    health: Arc<Health>,
    metrics: Arc<Metrics>,
    stop: CancellationToken,
) -> HeartbeatExit {
    let flush_stop = stop.child_token();
    let _flush_guard = flush_stop.clone().drop_guard();
    let writer_pool = pool.clone();
    tokio::spawn(flush_loop(
        running.clone(),
        process_id,
        move |fence, value: Value| {
            let pool = writer_pool.clone();
            async move { write_progress(&pool, fence, &value).await }
        },
        flush_stop,
    ));
    let min_lease_s = crate::job::secs_ceil(shortest_lease);
    beat_loop(
        running,
        process_id,
        every,
        shortest_lease.saturating_sub(every),
        health,
        metrics,
        stop,
        move |ids: Vec<i64>| {
            let pool = pool.clone();
            async move { heartbeat(&pool, process_id, &ids, min_lease_s).await }
        },
    )
    .await
}

/// The beat loop, with the database call injected so the timing is testable
/// without a database. `grace` is how long after the last successful beat the
/// handlers may keep running.
#[allow(clippy::too_many_arguments)]
async fn beat_loop<B, Fut>(
    running: Running,
    process_id: i64,
    every: Duration,
    grace: Duration,
    health: Arc<Health>,
    metrics: Arc<Metrics>,
    stop: CancellationToken,
    beat: B,
) -> HeartbeatExit
where
    B: Fn(Vec<i64>) -> Fut,
    Fut: Future<Output = Result<Beat, sqlx::Error>>,
{
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // After a give-up this re-arms one beat later, for as long as beats keep
    // failing: a job claimed meanwhile is never renewed and must be cancelled too.
    let mut give_up_at = Instant::now() + grace;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return HeartbeatExit::Stopped,
            () = tokio::time::sleep_until(give_up_at) => {
                give_up_at = give_up(&running, &metrics, every);
            }
            _ = ticker.tick() => {
                let ids = running.ids();
                let started = Instant::now();
                // A timeout is a failed beat. Dropping the call rolls its transaction back.
                let call = tokio::time::timeout(every, beat(ids.clone()));
                tokio::pin!(call);
                let outcome = loop {
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => return HeartbeatExit::Stopped,
                        () = tokio::time::sleep_until(give_up_at) => {
                            give_up_at = give_up(&running, &metrics, every);
                        }
                        outcome = &mut call => break outcome,
                    }
                };
                match outcome {
                    Ok(Ok(Beat::Pruned)) => {
                        tracing::error!(process_id, "process.self_fenced: registry row gone; stopping");
                        health.set_fenced();
                        metrics.lease_lost(running.cancel_all(CancelReason::LeaseLost));
                        return HeartbeatExit::SelfFenced;
                    }
                    Ok(Ok(Beat::Alive(rows))) => {
                        // The lease was renewed at some point after the call started.
                        give_up_at = started + grace;
                        health.beat_ok();
                        apply_lease_rows(&running, &metrics, &ids, &rows);
                    }
                    Ok(Err(err)) => {
                        metrics.db_error("heartbeat");
                        tracing::warn!(error = %err, "jobs: heartbeat failed");
                    }
                    Err(_) => {
                        metrics.db_error("heartbeat");
                        tracing::warn!("jobs: heartbeat timed out");
                    }
                }
            }
        }
    }
}

/// The give-up deadline passed: cancel every handler that is not cancelled yet
/// (logging each newly cancelled job once). Returns the next check, one beat
/// from now, so jobs claimed during the outage are caught too. Nothing running
/// means nothing to cancel.
fn give_up(running: &Running, metrics: &Metrics, every: Duration) -> Instant {
    let cancelled = running.cancel_pending(CancelReason::LeaseLost);
    metrics.lease_lost(cancelled.len() as u64);
    for id in cancelled {
        tracing::error!(
            job_id = id,
            "jobs.lease_expired: no heartbeat for a whole lease; cancelling handler"
        );
    }
    Instant::now() + every
}

/// Apply a successful beat. `ids` is the snapshot sent with that beat: ids in
/// it that were not renewed lost their lease; a job inserted after the
/// snapshot was never sent and must not be touched.
fn apply_lease_rows(running: &Running, metrics: &Metrics, ids: &[i64], rows: &[LeaseRow]) {
    let renewed: HashSet<i64> = rows.iter().map(|r| r.id).collect();
    for id in ids.iter().filter(|id| !renewed.contains(id)) {
        tracing::warn!(job_id = id, "jobs.lease_lost");
        // Counted once per lost lease: later beats see the same id again.
        if running.cancel(*id, CancelReason::LeaseLost) {
            metrics.lease_lost(1);
        }
    }
    for row in rows.iter().filter(|r| r.cancel_requested) {
        running.cancel(row.id, CancelReason::Requested);
    }
}

/// Write pending progress every [`FLUSH_EVERY`] until `stop`. A failed or
/// timed-out write puts the value back unless the handler reported a newer one.
async fn flush_loop<W, Fut>(running: Running, process_id: i64, write: W, stop: CancellationToken)
where
    W: Fn(Fence, Value) -> Fut,
    Fut: Future<Output = Result<bool, sqlx::Error>>,
{
    let mut ticker = tokio::time::interval(FLUSH_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            _ = ticker.tick() => {}
        }
        for (fence, value) in running.take_progress(process_id) {
            if stop.is_cancelled() {
                return;
            }
            match tokio::time::timeout(FLUSH_WRITE_TIMEOUT, write(fence, value.clone())).await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!(job_id = fence.job_id, error = %err, "jobs: progress write failed");
                    running.restore_progress(fence.job_id, fence.attempt, value);
                }
                Err(_) => {
                    tracing::warn!(job_id = fence.job_id, "jobs: progress write timed out");
                    running.restore_progress(fence.job_id, fence.attempt, value);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    use serde_json::json;

    use super::*;
    use crate::running::RunningEntry;

    fn entry(attempt: i32) -> RunningEntry {
        RunningEntry {
            attempt,
            token: CancellationToken::new(),
            cancel: Arc::new(crate::executor::CancelCell::default()),
            progress: Arc::new(Mutex::new(None)),
        }
    }

    async fn settle() {
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
    }

    /// Advance paused time in small steps: one big jump would re-base the
    /// loop's chained timers on the jumped-to instant.
    async fn advance_by(total: Duration) {
        for _ in 0..(total.as_millis() / 100) {
            tokio::time::advance(Duration::from_millis(100)).await;
            settle().await;
        }
    }

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

    #[test]
    fn lost_lease_diff_uses_the_snapshot_sent_with_the_beat() {
        let running = Running::default();
        let (a, b, c) = (entry(1), entry(1), entry(1));
        let (ca, cb, cc) = (a.cancel.clone(), b.cancel.clone(), c.cancel.clone());
        running.insert(1, a);
        running.insert(2, b);
        let snapshot = running.ids();
        // Claimed after the snapshot: the beat never saw it.
        running.insert(3, c);
        let rows = [LeaseRow {
            id: 1,
            cancel_requested: true,
        }];
        let metrics = Metrics::new();
        apply_lease_rows(&running, &metrics, &snapshot, &rows);
        assert_eq!(ca.get(), Some(CancelReason::Requested));
        assert_eq!(cb.get(), Some(CancelReason::LeaseLost));
        assert_eq!(cc.get(), None);
        // The same beat result again (the handler is still running): not counted twice.
        apply_lease_rows(&running, &metrics, &snapshot, &rows);
        assert!(metrics.encode().contains("jobs_lease_lost_total 1\n"));
    }

    type BeatFuture = std::pin::Pin<Box<dyn Future<Output = Result<Beat, sqlx::Error>> + Send>>;

    /// Beat 1 succeeds at t=0, every later call hangs forever.
    fn hanging_after_first() -> impl Fn(Vec<i64>) -> BeatFuture {
        let calls = Arc::new(AtomicUsize::new(0));
        move |ids| {
            let first = calls.fetch_add(1, Ordering::SeqCst) == 0;
            Box::pin(async move {
                if first {
                    // Renews every lease it was sent.
                    Ok(Beat::Alive(
                        ids.iter()
                            .map(|&id| LeaseRow {
                                id,
                                cancel_requested: false,
                            })
                            .collect(),
                    ))
                } else {
                    std::future::pending().await
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn gives_up_one_beat_before_the_lease_even_while_a_beat_hangs() {
        let running = Running::default();
        let e = entry(1);
        let (token, cell) = (e.token.clone(), e.cancel.clone());
        running.insert(1, e);
        let health = Arc::new(Health::default());
        let metrics = Arc::new(Metrics::new());
        let stop = CancellationToken::new();
        // every 1 s, lease 3 s: grace = 2 s after the last successful beat (t=0).
        let task = tokio::spawn(beat_loop(
            running.clone(),
            7,
            Duration::from_secs(1),
            Duration::from_secs(2),
            health.clone(),
            metrics.clone(),
            stop.clone(),
            hanging_after_first(),
        ));
        settle().await;
        assert!(health.last_beat_age().is_some());
        tokio::time::advance(Duration::from_millis(1990)).await;
        settle().await;
        assert!(
            !token.is_cancelled(),
            "must not cancel before last_ok + grace"
        );
        tokio::time::advance(Duration::from_millis(20)).await;
        settle().await;
        assert!(token.is_cancelled(), "must cancel at last_ok + grace");
        assert_eq!(cell.get(), Some(CancelReason::LeaseLost));
        // The give-up re-checks every beat, but the lost lease counts once.
        advance_by(Duration::from_millis(3000)).await;
        assert!(
            metrics.encode().contains("jobs_lease_lost_total 1\n"),
            "{}",
            metrics.encode()
        );
        // An outage cancels but never fences, and the task keeps beating.
        assert!(!health.is_fenced());
        assert!(!task.is_finished());
        // A stop is honoured while a beat call is still in flight.
        stop.cancel();
        settle().await;
        assert_eq!(task.await.unwrap(), HeartbeatExit::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn jobs_claimed_after_the_give_up_are_cancelled_at_the_next_recheck() {
        let running = Running::default();
        let (a, c) = (entry(1), entry(1));
        let (ca, cc) = (a.cancel.clone(), c.cancel.clone());
        running.insert(1, a);
        running.insert(3, c);
        // Already cancelled for another reason: the give-up must leave it alone.
        running.cancel(3, CancelReason::Requested);
        let stop = CancellationToken::new();
        let task = tokio::spawn(beat_loop(
            running.clone(),
            7,
            Duration::from_secs(1),
            Duration::from_secs(2),
            Arc::new(Health::default()),
            Arc::new(Metrics::new()),
            stop.clone(),
            hanging_after_first(),
        ));
        settle().await;
        tokio::time::advance(Duration::from_millis(2010)).await;
        settle().await;
        assert_eq!(ca.get(), Some(CancelReason::LeaseLost));
        assert_eq!(cc.get(), Some(CancelReason::Requested));
        // Claimed after the give-up fired (claims can still succeed while beats hang).
        let b = entry(1);
        let cb = b.cancel.clone();
        running.insert(2, b);
        tokio::time::advance(Duration::from_millis(900)).await;
        settle().await;
        assert_eq!(cb.get(), None, "not before the next re-check");
        tokio::time::advance(Duration::from_millis(200)).await;
        settle().await;
        assert_eq!(cb.get(), Some(CancelReason::LeaseLost));
        assert_eq!(ca.get(), Some(CancelReason::LeaseLost));
        assert_eq!(cc.get(), Some(CancelReason::Requested));
        stop.cancel();
        settle().await;
        assert_eq!(task.await.unwrap(), HeartbeatExit::Stopped);
    }

    #[derive(Clone, Copy)]
    enum Mode {
        Ok,
        Hang,
        Pruned,
    }

    /// A beat whose outcome is read from `mode` when the call starts.
    fn scripted(mode: Arc<Mutex<Mode>>) -> impl Fn(Vec<i64>) -> BeatFuture {
        move |ids| {
            let m = *mode
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            Box::pin(async move {
                match m {
                    Mode::Ok => Ok(Beat::Alive(
                        ids.iter()
                            .map(|&id| LeaseRow {
                                id,
                                cancel_requested: false,
                            })
                            .collect(),
                    )),
                    Mode::Hang => std::future::pending().await,
                    Mode::Pruned => Ok(Beat::Pruned),
                }
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_late_success_rearms_the_normal_deadline() {
        let mode = Arc::new(Mutex::new(Mode::Hang));
        let running = Running::default();
        let stop = CancellationToken::new();
        let task = tokio::spawn(beat_loop(
            running.clone(),
            7,
            Duration::from_secs(1),
            Duration::from_secs(2),
            Arc::new(Health::default()),
            Arc::new(Metrics::new()),
            stop.clone(),
            scripted(mode.clone()),
        ));
        settle().await;
        // The outage outlasts the give-up (t=2) and its re-checks.
        advance_by(Duration::from_millis(3500)).await;
        *mode.lock().unwrap() = Mode::Ok;
        // The next beat starts at t=4 and succeeds: the deadline is now t=6.
        advance_by(Duration::from_millis(600)).await;
        let d = entry(1);
        let cd = d.cancel.clone();
        running.insert(4, d);
        *mode.lock().unwrap() = Mode::Hang;
        advance_by(Duration::from_millis(1800)).await; // t=5.9
        assert_eq!(cd.get(), None, "not before success + grace");
        advance_by(Duration::from_millis(200)).await; // t=6.1
        assert_eq!(cd.get(), Some(CancelReason::LeaseLost));
        stop.cancel();
        settle().await;
        assert_eq!(task.await.unwrap(), HeartbeatExit::Stopped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pruned_registry_row_fences_and_cancels_everything() {
        let mode = Arc::new(Mutex::new(Mode::Pruned));
        let running = Running::default();
        let (a, b) = (entry(1), entry(2));
        let (ca, cb) = (a.cancel.clone(), b.cancel.clone());
        running.insert(1, a);
        running.insert(2, b);
        let health = Arc::new(Health::default());
        let metrics = Arc::new(Metrics::new());
        let task = tokio::spawn(beat_loop(
            running.clone(),
            7,
            Duration::from_secs(1),
            Duration::from_secs(2),
            health.clone(),
            metrics.clone(),
            CancellationToken::new(),
            scripted(mode),
        ));
        settle().await;
        assert_eq!(task.await.unwrap(), HeartbeatExit::SelfFenced);
        assert!(health.is_fenced());
        assert_eq!(ca.get(), Some(CancelReason::LeaseLost));
        assert_eq!(cb.get(), Some(CancelReason::LeaseLost));
        assert!(metrics.encode().contains("jobs_lease_lost_total 2\n"));
    }

    fn running_with_progress(value: Value) -> (Running, Arc<Mutex<Option<Value>>>) {
        let running = Running::default();
        let e = entry(1);
        *e.progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(value);
        let slot = e.progress.clone();
        running.insert(1, e);
        (running, slot)
    }

    #[tokio::test(start_paused = true)]
    async fn failed_progress_write_is_restored_and_flushed_again() {
        let (running, _slot) = running_with_progress(json!({"done": 1}));
        let written = Arc::new(Mutex::new(Vec::<Value>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let (w, c) = (written.clone(), calls.clone());
        let task = tokio::spawn(flush_loop(
            running.clone(),
            7,
            move |_fence, value: Value| {
                let (w, c) = (w.clone(), c.clone());
                async move {
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(sqlx::Error::PoolTimedOut);
                    }
                    w.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(value);
                    Ok(true)
                }
            },
            stop.clone(),
        ));
        settle().await; // first tick: the write fails and the value goes back
        assert!(
            written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
        tokio::time::advance(FLUSH_EVERY).await;
        settle().await;
        assert_eq!(
            *written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![json!({"done": 1})]
        );
        stop.cancel();
        task.await.unwrap_or_default();
    }

    #[tokio::test(start_paused = true)]
    async fn a_newer_value_is_not_overwritten_by_a_restore() {
        let (running, slot) = running_with_progress(json!("old"));
        let stop = CancellationToken::new();
        let s = slot.clone();
        let task = tokio::spawn(flush_loop(
            running.clone(),
            7,
            move |_fence, _value: Value| {
                let s = s.clone();
                async move {
                    // The handler reports again while the write is in flight, which then fails.
                    *s.lock().unwrap_or_else(std::sync::PoisonError::into_inner) =
                        Some(json!("new"));
                    Err(sqlx::Error::PoolTimedOut)
                }
            },
            stop.clone(),
        ));
        settle().await;
        assert_eq!(
            *slot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some(json!("new"))
        );
        stop.cancel();
        task.await.unwrap_or_default();
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_progress_write_times_out_and_is_restored() {
        let (running, _slot) = running_with_progress(json!("p"));
        let written = Arc::new(Mutex::new(Vec::<Value>::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let stop = CancellationToken::new();
        let (w, c) = (written.clone(), calls.clone());
        // The first write hangs, later ones succeed.
        let task = tokio::spawn(flush_loop(
            running.clone(),
            7,
            move |_fence, value: Value| {
                let (w, c) = (w.clone(), c.clone());
                async move {
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        return std::future::pending().await;
                    }
                    w.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(value);
                    Ok(true)
                }
            },
            stop.clone(),
        ));
        settle().await;
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        tokio::time::advance(FLUSH_WRITE_TIMEOUT + Duration::from_millis(1)).await;
        settle().await;
        tokio::time::advance(FLUSH_EVERY).await;
        settle().await;
        assert_eq!(
            *written
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![json!("p")]
        );
        stop.cancel();
        task.await.unwrap_or_default();
    }
}

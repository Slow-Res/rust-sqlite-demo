//! A worker process (spec §11, §13): one claim loop per queue, one LISTEN
//! connection, one heartbeat task, and (Task 10) the maintenance candidate.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::config::{ConfigError, QueueConfig, WorkerConfig};
use crate::enqueue::NOTIFY_CHANNEL;
use crate::executor::{CancelCell, CancelReason, Execution, execute};
use crate::heartbeat::{self, Health, HeartbeatExit};
use crate::metrics::Metrics;
use crate::rate_limit::RateLimiter;
use crate::registry::{Registry, RegistryError};
use crate::running::{Running, RunningEntry};
use crate::store::claim::{ClaimRequest, ClaimedJob, claim};
use crate::store::process::{ProcessInfo, deregister, register, set_stopping};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Registry(#[from] RegistryError),
    #[error("database: {0}")]
    Db(#[from] sqlx::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Shutdown,
    /// The registry row was deleted (presumed dead): exit non-zero and let the
    /// supervisor restart the process (spec §8.2).
    SelfFenced,
}

pub struct Worker {
    pool: PgPool,
    registry: Registry,
    config: WorkerConfig,
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    limiter: Arc<RateLimiter>,
}

/// What every queue loop and job task shares.
#[derive(Clone)]
struct Shared {
    pool: PgPool,
    registry: Arc<Registry>,
    running: Running,
    tracker: TaskTracker,
    limiter: Arc<RateLimiter>,
    metrics: Arc<Metrics>,
    process_id: i64,
    poll: Duration,
    health: Arc<Health>,
    /// How long after the last good beat claiming stops: the shortest lease
    /// minus one beat, the point where the heartbeat cancels handlers.
    grace: Duration,
}

impl Worker {
    pub fn new(
        pool: PgPool,
        registry: Registry,
        config: WorkerConfig,
    ) -> Result<Self, WorkerError> {
        config.validate()?;
        registry.validate(&config)?;
        Ok(Self {
            pool,
            registry,
            config,
            metrics: Arc::new(Metrics::new()),
            health: Arc::new(Health::default()),
            limiter: Arc::new(RateLimiter::new()),
        })
    }

    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    pub fn health(&self) -> Arc<Health> {
        self.health.clone()
    }

    /// Run until `shutdown` fires (graceful, spec §13) or the process is fenced.
    pub async fn run(self, shutdown: CancellationToken) -> Result<ExitReason, WorkerError> {
        let Worker {
            pool,
            registry,
            config,
            metrics,
            health,
            limiter,
        } = self;
        let registry = Arc::new(registry);
        let info = process_info(&config, &registry);
        let Some(process_id) = register_until_ok(&pool, &info, &metrics, &shutdown).await else {
            return Ok(ExitReason::Shutdown);
        };
        tracing::info!(process_id, name = %info.name, "process.started");

        let running = Running::default();
        let hb_stop = CancellationToken::new();
        let shortest_lease = registry.min_lease().unwrap_or(config.heartbeat * 3);
        let mut hb = tokio::spawn(heartbeat::run(
            pool.clone(),
            process_id,
            running.clone(),
            config.heartbeat,
            shortest_lease,
            health.clone(),
            hb_stop.clone(),
        ));

        let claim_stop = CancellationToken::new();
        let tracker = TaskTracker::new();
        let shared = Shared {
            pool: pool.clone(),
            registry: registry.clone(),
            running: running.clone(),
            tracker: tracker.clone(),
            limiter,
            metrics: metrics.clone(),
            process_id,
            poll: config.poll,
            health: health.clone(),
            grace: shortest_lease.saturating_sub(config.heartbeat),
        };
        let wakes: HashMap<String, Arc<Notify>> = config
            .queues
            .iter()
            .map(|q| (q.name.clone(), Arc::new(Notify::new())))
            .collect();
        let mut helpers = JoinSet::new();
        if config.listen {
            helpers.spawn(listen(
                pool.clone(),
                wakes.clone(),
                metrics.clone(),
                claim_stop.clone(),
            ));
        }
        for q in &config.queues {
            let wake = wakes.get(&q.name).cloned().unwrap_or_default();
            helpers.spawn(queue_loop(
                shared.clone(),
                q.clone(),
                wake,
                claim_stop.clone(),
            ));
        }
        let maintenance_stop = CancellationToken::new();
        // Task 10 spawns the maintenance candidate here.

        let reason = tokio::select! {
            () = shutdown.cancelled() => ExitReason::Shutdown,
            exit = &mut hb => match exit {
                Ok(HeartbeatExit::Stopped) => ExitReason::Shutdown,
                // Fenced, or the heartbeat task died: either way it is unsafe to go on.
                Ok(HeartbeatExit::SelfFenced) | Err(_) => ExitReason::SelfFenced,
            },
        };

        claim_stop.cancel();
        maintenance_stop.cancel();
        if reason == ExitReason::Shutdown {
            tracing::info!(process_id, "process.stopping");
            if let Err(err) = set_stopping(&pool, process_id).await {
                tracing::warn!(error = %err, "jobs: could not mark the process as stopping");
            }
        }
        while helpers.join_next().await.is_some() {}
        tracker.close();

        if reason == ExitReason::SelfFenced {
            // The heartbeat cancels with LeaseLost when it sees the fence, but not when
            // it died (panicked); the first reason wins, so this is idempotent.
            // Their writes will be fenced off.
            running.cancel_all(CancelReason::LeaseLost);
            let _ = tokio::time::timeout(config.cancel_grace, tracker.wait()).await;
            return Ok(ExitReason::SelfFenced);
        }

        if tokio::time::timeout(config.shutdown_timeout, tracker.wait())
            .await
            .is_err()
        {
            running.cancel_all(CancelReason::Shutdown);
            let _ = tokio::time::timeout(config.cancel_grace, tracker.wait()).await;
        }
        hb_stop.cancel();
        let _ = hb.await;
        if !running.is_empty() {
            tracing::warn!(
                count = running.len(),
                "jobs: handlers ignored cancellation; their leases will expire and the reaper will retry them"
            );
        }
        if let Err(err) = deregister(&pool, process_id).await {
            tracing::warn!(error = %err, "jobs: could not delete the registry row; the pruner will");
        }
        tracing::info!(process_id, "process.stopped");
        Ok(ExitReason::Shutdown)
    }
}

fn process_info(config: &WorkerConfig, registry: &Registry) -> ProcessInfo {
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    let pid = std::process::id();
    let queues: Vec<_> = config
        .queues
        .iter()
        .map(|q| json!({ "name": q.name, "slots": q.slots, "max_running": q.max_running }))
        .collect();
    ProcessInfo {
        name: format!(
            "{}-{hostname}-{pid}-{:08x}",
            config.process_kind,
            rand::random::<u32>()
        ),
        kind: config.process_kind.clone(),
        hostname,
        pid: i32::try_from(pid).unwrap_or(i32::MAX),
        version: config.version.clone(),
        config: json!({ "queues": queues, "kinds": registry.kinds() }),
    }
}

async fn register_until_ok(
    pool: &PgPool,
    info: &ProcessInfo,
    metrics: &Metrics,
    shutdown: &CancellationToken,
) -> Option<i64> {
    let mut wait = Duration::from_secs(1);
    loop {
        match register(pool, info).await {
            Ok(id) => return Some(id),
            Err(err) => {
                metrics.db_error("register");
                tracing::warn!(error = %err, "jobs: could not register the process; retrying")
            }
        }
        tokio::select! {
            () = shutdown.cancelled() => return None,
            () = tokio::time::sleep(wait) => {}
        }
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

/// True while the last successful beat is older than `grace`. Before the first
/// beat there is nothing to compare with, which counts as fresh.
fn heartbeat_stale(health: &Health, grace: Duration) -> bool {
    health.last_beat_age().is_some_and(|age| age > grace)
}

fn jittered(d: Duration) -> Duration {
    d.mul_f64(rand::random_range(0.9..=1.1))
}

async fn queue_loop(s: Shared, q: QueueConfig, wake: Arc<Notify>, stop: CancellationToken) {
    let kinds = s.registry.kinds_for_queue(&q.name);
    if kinds.is_empty() {
        tracing::warn!(queue = %q.name, "jobs: no registered kind uses this queue; not claiming from it");
        return;
    }
    let slots = Arc::new(Semaphore::new(q.slots));
    s.metrics
        .set_slots(&q.name, i64::try_from(q.slots).unwrap_or(i64::MAX));
    let mut err_wait = Duration::from_secs(1);
    loop {
        if stop.is_cancelled() {
            return;
        }
        let free = slots.available_permits();
        let mut full = false;
        // A job claimed while beats keep failing would never be renewed and
        // would just be cancelled again, so wait out the outage first.
        let stale = heartbeat_stale(&s.health, s.grace);
        if stale {
            tracing::debug!(queue = %q.name, "jobs: heartbeat is stale; not claiming");
        }
        if free > 0 && !stale {
            let req = ClaimRequest {
                queue: &q.name,
                kinds: &kinds,
                process_id: s.process_id,
                limit: i64::try_from(free).unwrap_or(i64::MAX),
                max_running: q.max_running,
            };
            match claim(&s.pool, &req).await {
                Ok(jobs) => {
                    err_wait = Duration::from_secs(1);
                    full = jobs.len() == free;
                    s.metrics.claimed(&q.name, jobs.len() as u64);
                    for job in jobs {
                        // Only this loop takes permits, and it claimed at most `free`
                        // jobs, so a permit is always there.
                        let Ok(permit) = slots.clone().try_acquire_owned() else {
                            tracing::error!(
                                job_id = job.id,
                                "jobs: no free slot for a claimed job; its lease will expire"
                            );
                            continue;
                        };
                        spawn_job(&s, &q.name, job, permit, wake.clone());
                    }
                }
                Err(err) => {
                    s.metrics.db_error("claim");
                    tracing::warn!(queue = %q.name, error = %err, "jobs: claim failed");
                    tokio::select! {
                        () = stop.cancelled() => return,
                        () = tokio::time::sleep(err_wait) => {}
                    }
                    err_wait = (err_wait * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        }
        if full && slots.available_permits() > 0 {
            continue; // more jobs are probably waiting
        }
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep(jittered(s.poll)) => {}
            () = stop.cancelled() => return,
        }
    }
}

/// Owns everything a running job holds in this process, so it is released on
/// every path, a panic included: the slot, the `Running` entry (which the
/// heartbeat would keep renewing), the gauge, and a wake-up for the loop.
struct JobGuard {
    running: Running,
    metrics: Arc<Metrics>,
    queue: String,
    id: i64,
    wake: Arc<Notify>,
    _permit: OwnedSemaphorePermit,
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        self.running.remove(self.id);
        self.metrics.running_add(&self.queue, -1);
        self.wake.notify_one();
    }
}

fn spawn_job(
    s: &Shared,
    queue: &str,
    job: ClaimedJob,
    permit: OwnedSemaphorePermit,
    wake: Arc<Notify>,
) {
    let token = CancellationToken::new();
    let cancel = Arc::new(CancelCell::default());
    let progress = Arc::new(Mutex::new(None));
    s.running.insert(
        job.id,
        RunningEntry {
            attempt: job.attempt,
            token: token.clone(),
            cancel: cancel.clone(),
            progress: progress.clone(),
        },
    );
    s.metrics.running_add(queue, 1);
    let guard = JobGuard {
        running: s.running.clone(),
        metrics: s.metrics.clone(),
        queue: queue.to_owned(),
        id: job.id,
        wake,
        _permit: permit,
    };
    let s = s.clone();
    let tracker = s.tracker.clone();
    tracker.spawn(async move {
        let _guard = guard;
        let (id, kind) = (job.id, job.kind.clone());
        tracing::debug!(job_id = id, %kind, attempt = job.attempt, "jobs.claimed");
        s.metrics.waited(&kind, job.waited_s);
        let started = tokio::time::Instant::now();
        let exec = Execution {
            job,
            process_id: s.process_id,
            token,
            cancel,
            progress,
        };
        let finished = execute(&s.pool, &s.registry, s.limiter.clone(), exec).await;
        tracing::debug!(job_id = id, %kind, outcome = finished.as_str(), "jobs.finished");
        s.metrics.finished(&kind, finished, started.elapsed());
    });
}

/// LISTEN for enqueue notifications and wake the matching queue loop. Lost
/// notifications only cost latency: every loop also polls.
async fn listen(
    pool: PgPool,
    wakes: HashMap<String, Arc<Notify>>,
    metrics: Arc<Metrics>,
    stop: CancellationToken,
) {
    // How long a listener must live before a later failure counts as a new outage.
    const HEALTHY_AFTER: Duration = Duration::from_secs(30);
    let mut wait = Duration::from_secs(1);
    loop {
        let connect = async {
            let mut l = PgListener::connect_with(&pool).await?;
            l.listen(NOTIFY_CHANNEL).await?;
            Ok::<_, sqlx::Error>(l)
        };
        let mut listener = tokio::select! {
            () = stop.cancelled() => return,
            r = connect => match r {
                Ok(l) => l,
                Err(err) => {
                    metrics.db_error("listen");
                    tracing::warn!(error = %err, "jobs: LISTEN failed; polling meanwhile");
                    tokio::select! { () = stop.cancelled() => return, () = tokio::time::sleep(wait) => {} }
                    wait = (wait * 2).min(Duration::from_secs(30));
                    continue;
                }
            },
        };
        // Notifications sent while there was no listener are gone: look once.
        wakes.values().for_each(|w| w.notify_one());
        let connected = tokio::time::Instant::now();
        loop {
            tokio::select! {
                () = stop.cancelled() => return,
                note = listener.try_recv() => match note {
                    Ok(Some(n)) => {
                        if let Some(w) = wakes.get(n.payload()) {
                            w.notify_one();
                        }
                    }
                    // The connection dropped (sqlx reconnects on the next call);
                    // notifications may have been missed, so wake everyone once.
                    Ok(None) => wakes.values().for_each(|w| w.notify_one()),
                    Err(err) => {
                        metrics.db_error("listen");
                        tracing::warn!(error = %err, "jobs: LISTEN connection failed");
                        break;
                    }
                },
            }
        }
        // Back off before reconnecting, so a flapping connection cannot spin.
        if connected.elapsed() >= HEALTHY_AFTER {
            wait = Duration::from_secs(1);
        }
        tokio::select! { () = stop.cancelled() => return, () = tokio::time::sleep(wait) => {} }
        wait = (wait * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_claim_gate_closes_only_when_the_last_beat_is_older_than_the_grace() {
        let health = Health::default();
        assert!(
            !heartbeat_stale(&health, Duration::ZERO),
            "no beat yet is not stale"
        );
        health.beat_ok();
        assert!(
            !heartbeat_stale(&health, Duration::from_secs(60)),
            "a fresh beat is not stale"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert!(
            heartbeat_stale(&health, Duration::from_millis(10)),
            "an old beat is stale"
        );
    }
}

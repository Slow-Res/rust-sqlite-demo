//! Prometheus metrics (spec §17.2). Per-process counters and histograms, plus
//! queue gauges that only the maintenance leader sets (one aggregate query for
//! the cluster instead of one per process).

use std::sync::atomic::AtomicU64;
use std::time::Duration;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::executor::Finished;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct QueueLabel {
    pub queue: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct KindLabel {
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OutcomeLabel {
    pub kind: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OpLabel {
    pub op: String,
}

type HistFamily = Family<KindLabel, Histogram, fn() -> Histogram>;

fn duration_buckets() -> Histogram {
    // 5 ms to about 22 min.
    Histogram::new(exponential_buckets(0.005, 2.0, 18))
}

fn wait_buckets() -> Histogram {
    Histogram::new(exponential_buckets(0.01, 2.0, 18))
}

pub struct Metrics {
    registry: Registry,
    claimed: Family<QueueLabel, Counter>,
    completed: Family<OutcomeLabel, Counter>,
    duration: HistFamily,
    wait: HistFamily,
    fence_rejected: Counter,
    lease_lost: Counter,
    db_errors: Family<OpLabel, Counter>,
    running: Family<QueueLabel, Gauge>,
    slots: Family<QueueLabel, Gauge>,
    available: Family<QueueLabel, Gauge>,
    scheduled: Family<QueueLabel, Gauge>,
    running_all: Family<QueueLabel, Gauge>,
    oldest_available: Family<QueueLabel, Gauge<f64, AtomicU64>>,
    failed_recent: Family<KindLabel, Gauge>,
    processes_live: Gauge,
    leader: Gauge,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let m = Self {
            claimed: Family::default(),
            completed: Family::default(),
            duration: Family::new_with_constructor(duration_buckets as fn() -> Histogram),
            wait: Family::new_with_constructor(wait_buckets as fn() -> Histogram),
            fence_rejected: Counter::default(),
            lease_lost: Counter::default(),
            db_errors: Family::default(),
            running: Family::default(),
            slots: Family::default(),
            available: Family::default(),
            scheduled: Family::default(),
            running_all: Family::default(),
            oldest_available: Family::default(),
            failed_recent: Family::default(),
            processes_live: Gauge::default(),
            leader: Gauge::default(),
            registry: Registry::default(),
        };
        registry.register(
            "jobs_claimed",
            "Jobs claimed by this process",
            m.claimed.clone(),
        );
        registry.register(
            "jobs_completed",
            "Attempts finished by this process, by outcome",
            m.completed.clone(),
        );
        registry.register(
            "jobs_duration_seconds",
            "Handler run time",
            m.duration.clone(),
        );
        registry.register(
            "jobs_wait_seconds",
            "Time from run_at to claim",
            m.wait.clone(),
        );
        registry.register(
            "jobs_fence_rejected",
            "Outcome writes rejected because the lease was lost",
            m.fence_rejected.clone(),
        );
        registry.register(
            "jobs_lease_lost",
            "Handlers cancelled because their lease was lost",
            m.lease_lost.clone(),
        );
        registry.register(
            "jobs_db_errors",
            "Database errors in the worker loops, by operation",
            m.db_errors.clone(),
        );
        registry.register(
            "jobs_running",
            "Handlers running in this process",
            m.running.clone(),
        );
        registry.register(
            "jobs_slots",
            "Handler slots of this process",
            m.slots.clone(),
        );
        registry.register(
            "jobs_available",
            "Jobs ready to run (leader only)",
            m.available.clone(),
        );
        registry.register(
            "jobs_scheduled",
            "Jobs waiting for run_at (leader only)",
            m.scheduled.clone(),
        );
        registry.register(
            "jobs_running_all",
            "Running jobs across all processes (leader only)",
            m.running_all.clone(),
        );
        registry.register(
            "jobs_oldest_available_seconds",
            "Age of the oldest ready job: queue latency (leader only)",
            m.oldest_available.clone(),
        );
        registry.register(
            "jobs_failed_recent",
            "Jobs failed in the last 24 h (leader only)",
            m.failed_recent.clone(),
        );
        registry.register(
            "jobs_processes_live",
            "Processes with a recent heartbeat (leader only)",
            m.processes_live.clone(),
        );
        registry.register(
            "jobs_leader",
            "1 while this process is the maintenance leader",
            m.leader.clone(),
        );
        Self { registry, ..m }
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        if let Err(err) = encode(&mut out, &self.registry) {
            tracing::error!(error = ?err, "encoding metrics failed");
        }
        out
    }

    fn q(queue: &str) -> QueueLabel {
        QueueLabel {
            queue: queue.to_owned(),
        }
    }

    pub(crate) fn claimed(&self, queue: &str, n: u64) {
        self.claimed.get_or_create(&Self::q(queue)).inc_by(n);
    }

    pub(crate) fn waited(&self, kind: &str, secs: f64) {
        self.wait
            .get_or_create(&KindLabel {
                kind: kind.to_owned(),
            })
            .observe(secs.max(0.0));
    }

    pub(crate) fn finished(&self, kind: &str, finished: Finished, took: Duration) {
        let label = OutcomeLabel {
            kind: kind.to_owned(),
            outcome: finished.as_str().to_owned(),
        };
        self.completed.get_or_create(&label).inc();
        self.duration
            .get_or_create(&KindLabel {
                kind: kind.to_owned(),
            })
            .observe(took.as_secs_f64());
        match finished {
            Finished::FenceRejected => {
                self.fence_rejected.inc();
            }
            Finished::LeaseLost => {
                self.lease_lost.inc();
            }
            _ => {}
        }
    }

    pub(crate) fn db_error(&self, op: &str) {
        self.db_errors
            .get_or_create(&OpLabel { op: op.to_owned() })
            .inc();
    }

    pub(crate) fn running_add(&self, queue: &str, delta: i64) {
        self.running.get_or_create(&Self::q(queue)).inc_by(delta);
    }

    pub(crate) fn set_slots(&self, queue: &str, n: i64) {
        self.slots.get_or_create(&Self::q(queue)).set(n);
    }

    pub(crate) fn set_queue_gauges(
        &self,
        queue: &str,
        available: i64,
        scheduled: i64,
        running: i64,
        oldest_s: f64,
    ) {
        let l = Self::q(queue);
        self.available.get_or_create(&l).set(available);
        self.scheduled.get_or_create(&l).set(scheduled);
        self.running_all.get_or_create(&l).set(running);
        self.oldest_available.get_or_create(&l).set(oldest_s);
    }

    pub(crate) fn set_failed_recent(&self, kind: &str, n: i64) {
        self.failed_recent
            .get_or_create(&KindLabel {
                kind: kind.to_owned(),
            })
            .set(n);
    }

    pub(crate) fn set_processes_live(&self, n: i64) {
        self.processes_live.set(n);
    }

    pub(crate) fn set_leader(&self, leader: bool) {
        self.leader.set(i64::from(leader));
    }

    /// Drop the leader-only series, so a former leader stops exporting stale values.
    pub(crate) fn clear_leader_gauges(&self) {
        self.available.clear();
        self.scheduled.clear();
        self.running_all.clear();
        self.oldest_available.clear();
        self.failed_recent.clear();
        self.processes_live.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_and_gauges_are_exported_with_labels() {
        let m = Metrics::new();
        m.finished("a.v1", Finished::Succeeded, Duration::from_millis(12));
        m.set_queue_gauges("email", 3, 1, 2, 4.5);
        let text = m.encode();
        assert!(
            text.contains(r#"jobs_completed_total{kind="a.v1",outcome="succeeded"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"jobs_available{queue="email"} 3"#),
            "{text}"
        );
        m.clear_leader_gauges();
        assert!(!m.encode().contains("jobs_available{"));
    }
}

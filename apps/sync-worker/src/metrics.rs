//! The worker's Prometheus metrics (spec §8) and the clock helpers they use.

use std::sync::atomic::AtomicU64;
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus_client::encoding::text::encode;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;

pub struct Metrics {
    registry: Registry,
    pub pending_changes: Gauge,
    pub lag_seconds: Gauge<f64, AtomicU64>,
    pub last_success_seconds: Gauge<f64, AtomicU64>,
    pub epoch: Gauge,
    // Counters get `_total` appended to their names when exposed.
    pub batches: Counter,
    pub rows_shipped: Counter,
    pub errors: Counter,
    pub resyncs: Counter,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let metrics = Self {
            pending_changes: Gauge::default(),
            lag_seconds: Gauge::default(),
            last_success_seconds: Gauge::default(),
            epoch: Gauge::default(),
            batches: Counter::default(),
            rows_shipped: Counter::default(),
            errors: Counter::default(),
            resyncs: Counter::default(),
            registry: Registry::default(),
        };
        registry.register(
            "sync_pending_changes",
            "Changes in SQLite not yet in Postgres",
            metrics.pending_changes.clone(),
        );
        registry.register(
            "sync_lag_seconds",
            "Age of the oldest change not yet in Postgres (0 when none)",
            metrics.lag_seconds.clone(),
        );
        registry.register(
            "sync_last_success_timestamp_seconds",
            "Unix time of the last successful sync cycle",
            metrics.last_success_seconds.clone(),
        );
        registry.register(
            "sync_epoch",
            "Epoch of this node's copy in Postgres",
            metrics.epoch.clone(),
        );
        registry.register(
            "sync_batches",
            "Batches applied to Postgres",
            metrics.batches.clone(),
        );
        registry.register(
            "sync_rows_shipped",
            "Rows inserted, updated or deleted in Postgres",
            metrics.rows_shipped.clone(),
        );
        registry.register(
            "sync_errors",
            "Failed sync sessions",
            metrics.errors.clone(),
        );
        registry.register(
            "sync_resyncs",
            "Full copies made because a restore was detected",
            metrics.resyncs.clone(),
        );
        Self {
            registry,
            ..metrics
        }
    }

    /// The metrics in Prometheus text format.
    pub fn encode(&self) -> String {
        let mut out = String::new();
        if let Err(err) = encode(&mut out, &self.registry) {
            tracing::error!(error = ?err, "encoding metrics failed");
        }
        out
    }

    /// A cycle succeeded within `max_age_secs` of `now_secs`.
    pub fn healthy(&self, now_secs: f64, max_age_secs: f64) -> bool {
        let last = self.last_success_seconds.get();
        last > 0.0 && now_secs - last <= max_age_secs
    }
}

/// Milliseconds since the Unix epoch (the unit of `change_log.created_at`).
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Age in seconds of the oldest unsent change; 0 when there is none.
pub fn lag_seconds(oldest_ms: Option<i64>, now_ms: i64) -> f64 {
    oldest_ms.map_or(0.0, |oldest| (now_ms - oldest).max(0) as f64 / 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_metric_is_exposed_by_name() {
        let m = Metrics::new();
        m.batches.inc();
        let text = m.encode();
        for name in [
            "sync_pending_changes",
            "sync_lag_seconds",
            "sync_last_success_timestamp_seconds",
            "sync_epoch",
            "sync_batches_total",
            "sync_rows_shipped_total",
            "sync_errors_total",
            "sync_resyncs_total",
        ] {
            assert!(text.contains(name), "{name} missing from:\n{text}");
        }
    }

    #[test]
    fn healthy_means_a_recent_success() {
        let m = Metrics::new();
        assert!(!m.healthy(1000.0, 60.0), "never succeeded");
        m.last_success_seconds.set(990.0);
        assert!(m.healthy(1000.0, 60.0));
        assert!(!m.healthy(1100.0, 60.0));
    }

    #[test]
    fn lag_is_zero_without_pending_changes() {
        assert_eq!(lag_seconds(None, 5000), 0.0);
        assert_eq!(lag_seconds(Some(3500), 5000), 1.5);
        assert_eq!(
            lag_seconds(Some(6000), 5000),
            0.0,
            "clock skew never gives a negative lag"
        );
    }
}

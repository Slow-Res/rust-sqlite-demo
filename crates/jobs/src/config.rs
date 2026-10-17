//! Worker configuration (spec §11, §18). The app parses environment variables
//! into these types; the crate never reads the environment itself.

use std::time::Duration;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0}")]
    Invalid(String),
}

/// One queue served by a process: `slots` handlers at once in this process,
/// and optionally at most `max_running` across all processes (spec §7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueConfig {
    pub name: String,
    pub slots: usize,
    pub max_running: Option<i64>,
}

const MAX_SLOTS: usize = 1024;

impl QueueConfig {
    /// Parse `JOBS_QUEUES`: comma-separated `name:slots[:max_running]`, e.g.
    /// `email:8,webhooks:16,imports:2:2`. Names are 1-64 of `a-z 0-9 _ -`.
    pub fn parse_list(raw: &str) -> Result<Vec<QueueConfig>, ConfigError> {
        let mut out: Vec<QueueConfig> = Vec::new();
        for item in raw.split(',').map(str::trim) {
            let parts: Vec<&str> = item.split(':').collect();
            let (name, slots, max) = match parts.as_slice() {
                [n, s] => (*n, *s, None),
                [n, s, m] => (*n, *s, Some(*m)),
                _ => {
                    return Err(invalid(format!(
                        "queue `{item}` must be name:slots or name:slots:max_running"
                    )));
                }
            };
            let name_ok = !name.is_empty()
                && name.len() <= 64
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
            if !name_ok {
                return Err(invalid(format!(
                    "queue name `{name}` must be 1-64 of a-z, 0-9, _ and -"
                )));
            }
            let slots: usize = slots
                .parse()
                .ok()
                .filter(|n| (1..=MAX_SLOTS).contains(n))
                .ok_or_else(|| {
                    invalid(format!(
                        "slots of `{name}` must be 1-{MAX_SLOTS}, got `{slots}`"
                    ))
                })?;
            let max_running = match max {
                None => None,
                Some(m) => Some(
                    m.parse::<i64>()
                        .ok()
                        .filter(|n| (1..=1_000_000).contains(n))
                        .ok_or_else(|| {
                            invalid(format!(
                                "max_running of `{name}` must be 1-1000000, got `{m}`"
                            ))
                        })?,
                ),
            };
            if out.iter().any(|q| q.name == name) {
                return Err(invalid(format!("queue `{name}` is listed twice")));
            }
            out.push(QueueConfig {
                name: name.to_owned(),
                slots,
                max_running,
            });
        }
        Ok(out)
    }
}

fn invalid(msg: String) -> ConfigError {
    ConfigError::Invalid(msg)
}

/// The elected leader's periodic work (spec §8.5, §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceConfig {
    /// Take part in leader election. Off only in tests that need a passive worker.
    pub enabled: bool,
    /// How often a non-leader tries the lock, and the leader checks its connection.
    pub leader_retry: Duration,
    pub reap_every: Duration,
    pub prune_every: Duration,
    /// A process silent this long is presumed dead (default 3 × the 60 s lease).
    pub prune_after: Duration,
    pub recurring_every: Duration,
    pub retention_every: Duration,
    pub keep_succeeded: Duration,
    pub keep_failed: Duration,
    pub gauges_every: Duration,
}

impl Default for MaintenanceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            leader_retry: Duration::from_secs(10),
            reap_every: Duration::from_secs(5),
            prune_every: Duration::from_secs(30),
            prune_after: Duration::from_secs(180),
            recurring_every: Duration::from_secs(1),
            retention_every: Duration::from_secs(60),
            keep_succeeded: Duration::from_secs(24 * 3600),
            keep_failed: Duration::from_secs(14 * 24 * 3600),
            gauges_every: Duration::from_secs(15),
        }
    }
}

/// Everything a [`crate::Worker`] process needs besides the pool and registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    pub queues: Vec<QueueConfig>,
    /// Fallback poll per queue (±10 % jitter); NOTIFY makes it rarely matter.
    pub poll: Duration,
    /// LISTEN for enqueue notifications.
    pub listen: bool,
    /// Heartbeat period; every kind's lease must be at least 3 × this (spec §8.2).
    pub heartbeat: Duration,
    pub shutdown_timeout: Duration,
    /// After cancelling handlers at shutdown, how long to wait for them (spec §13 step 3).
    pub cancel_grace: Duration,
    /// `worker` or `embedded`, stored in the registry row.
    pub process_kind: String,
    /// Crate version + git sha, stored in the registry row.
    pub version: String,
    pub maintenance: MaintenanceConfig,
}

impl WorkerConfig {
    pub fn new(queues: Vec<QueueConfig>) -> Self {
        Self {
            queues,
            poll: Duration::from_secs(1),
            listen: true,
            heartbeat: Duration::from_secs(20),
            shutdown_timeout: Duration::from_secs(25),
            cancel_grace: Duration::from_secs(5),
            process_kind: "worker".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            maintenance: MaintenanceConfig::default(),
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.queues.is_empty() {
            return Err(invalid("a worker must serve at least one queue".into()));
        }
        for (what, d) in [("poll", self.poll), ("heartbeat", self.heartbeat)] {
            if d.is_zero() {
                return Err(invalid(format!("{what} must be greater than zero")));
            }
        }
        let three_beats = self
            .heartbeat
            .checked_mul(3)
            .ok_or_else(|| invalid("heartbeat is too large".into()))?;
        if self.maintenance.enabled {
            let m = &self.maintenance;
            // `tokio::time::interval` panics on a zero period.
            for (what, d) in [
                ("leader_retry", m.leader_retry),
                ("reap_every", m.reap_every),
                ("prune_every", m.prune_every),
                ("recurring_every", m.recurring_every),
                ("retention_every", m.retention_every),
                ("gauges_every", m.gauges_every),
            ] {
                if d.is_zero() {
                    return Err(invalid(format!("{what} must be greater than zero")));
                }
            }
            if m.prune_after < three_beats {
                return Err(invalid("prune_after must be at least 3 heartbeats".into()));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_slots_and_optional_limits() {
        let qs = QueueConfig::parse_list(" email:8, webhooks:16 ,imports:2:2").unwrap();
        assert_eq!(
            qs,
            vec![
                QueueConfig {
                    name: "email".into(),
                    slots: 8,
                    max_running: None
                },
                QueueConfig {
                    name: "webhooks".into(),
                    slots: 16,
                    max_running: None
                },
                QueueConfig {
                    name: "imports".into(),
                    slots: 2,
                    max_running: Some(2)
                },
            ]
        );
    }

    #[test]
    fn rejects_bad_queue_lists() {
        for bad in [
            "",
            "email",
            "email:0",
            "email:x",
            "email:2:0",
            "Email:2",
            "a:1,a:2",
            "email:2000",
            "e mail:1",
        ] {
            assert!(QueueConfig::parse_list(bad).is_err(), "accepted `{bad}`");
        }
    }

    #[test]
    fn worker_config_validation() {
        let mut c = WorkerConfig::new(QueueConfig::parse_list("default:4").unwrap());
        assert!(c.validate().is_ok());
        c.heartbeat = Duration::ZERO;
        assert!(c.validate().is_err());
        let mut c = WorkerConfig::new(QueueConfig::parse_list("default:4").unwrap());
        c.maintenance.prune_after = Duration::from_secs(59);
        assert!(c.validate().is_err(), "prune_after under 3 heartbeats");
        let mut c = WorkerConfig::new(vec![]);
        c.maintenance.enabled = false;
        assert!(
            c.validate().is_err(),
            "a worker must serve at least one queue"
        );
    }

    #[test]
    fn zero_maintenance_intervals_are_rejected() {
        type Set = fn(&mut MaintenanceConfig);
        let setters: [(&str, Set); 6] = [
            ("leader_retry", |m| m.leader_retry = Duration::ZERO),
            ("reap_every", |m| m.reap_every = Duration::ZERO),
            ("prune_every", |m| m.prune_every = Duration::ZERO),
            ("recurring_every", |m| m.recurring_every = Duration::ZERO),
            ("retention_every", |m| m.retention_every = Duration::ZERO),
            ("gauges_every", |m| m.gauges_every = Duration::ZERO),
        ];
        for (name, set) in setters {
            let mut c = WorkerConfig::new(QueueConfig::parse_list("default:4").unwrap());
            set(&mut c.maintenance);
            let err = c.validate().unwrap_err();
            assert!(err.to_string().contains(name), "{name}: {err}");
            // A passive worker never runs the loops, so the value is harmless.
            c.maintenance.enabled = false;
            assert!(c.validate().is_ok(), "{name} with maintenance off");
        }
    }

    #[test]
    fn a_huge_heartbeat_is_rejected_without_overflow() {
        let mut c = WorkerConfig::new(QueueConfig::parse_list("default:4").unwrap());
        c.heartbeat = Duration::MAX;
        assert!(c.validate().is_err());
        c.maintenance.enabled = false;
        assert!(c.validate().is_err());
    }
}

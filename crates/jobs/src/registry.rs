//! Kind name -> decoder + handler, recurring tasks, and handler dependencies.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::config::WorkerConfig;
use crate::context::{Deps, JobContext};
use crate::enqueue::{EnqueueError, RawJob};
use crate::job::{Job, JobError, JobOptions};

pub(crate) type BoxFuture = Pin<Box<dyn Future<Output = Result<(), JobError>> + Send>>;
pub(crate) type HandlerFn =
    Arc<dyn Fn(Value, JobContext) -> Result<BoxFuture, serde_json::Error> + Send + Sync>;

#[derive(Clone)]
pub(crate) struct KindEntry {
    pub(crate) queue: &'static str,
    pub(crate) options: JobOptions,
    pub(crate) handler: HandlerFn,
}

/// A task enqueued by the leader once per `every` (spec §14).
#[derive(Clone)]
pub(crate) struct Recurring {
    pub(crate) task: &'static str,
    pub(crate) every: Duration,
    pub(crate) kind: &'static str,
    pub(crate) make: Arc<dyn Fn() -> Result<RawJob, EnqueueError> + Send + Sync>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("invalid job registry: {}", .0.join("; "))]
pub struct RegistryError(pub Vec<String>);

#[derive(Clone, Default)]
pub struct Registry {
    kinds: HashMap<&'static str, KindEntry>,
    duplicates: Vec<&'static str>,
    recurring: Vec<Recurring>,
    deps: Deps,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<J: Job>(mut self) -> Self {
        let handler: HandlerFn = Arc::new(|args, ctx| {
            let job: J = serde_json::from_value(args)?;
            Ok(Box::pin(job.perform(ctx)) as BoxFuture)
        });
        let entry = KindEntry {
            queue: J::QUEUE,
            options: J::options(),
            handler,
        };
        if self.kinds.insert(J::KIND, entry).is_some() {
            self.duplicates.push(J::KIND);
        }
        self
    }

    /// Enqueue `make()` once per `every`, aligned to multiples of `every` since
    /// the Unix epoch in database time. Missed ticks are not replayed.
    pub fn recurring<J: Job>(
        mut self,
        task: &'static str,
        every: Duration,
        make: impl Fn() -> J + Send + Sync + 'static,
    ) -> Self {
        self.recurring.push(Recurring {
            task,
            every,
            kind: J::KIND,
            make: Arc::new(move || RawJob::encode(&make())),
        });
        self
    }

    /// Make `value` available to handlers through [`JobContext::require`].
    pub fn provide<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.deps.insert(value);
        self
    }

    pub fn kinds(&self) -> Vec<String> {
        let mut k: Vec<String> = self.kinds.keys().map(|s| (*s).to_owned()).collect();
        k.sort();
        k
    }

    pub fn kinds_for_queue(&self, queue: &str) -> Vec<String> {
        let mut k: Vec<String> = self
            .kinds
            .iter()
            .filter(|(_, e)| e.queue == queue)
            .map(|(k, _)| (*k).to_owned())
            .collect();
        k.sort();
        k
    }

    /// The shortest lease of any registered kind: after this long without a
    /// successful heartbeat, all handlers must stop (spec §8.2).
    pub(crate) fn min_lease(&self) -> Option<Duration> {
        self.kinds.values().map(|e| e.options.lease).min()
    }

    pub(crate) fn get(&self, kind: &str) -> Option<&KindEntry> {
        self.kinds.get(kind)
    }

    pub(crate) fn recurring_tasks(&self) -> &[Recurring] {
        &self.recurring
    }

    pub(crate) fn deps(&self) -> Arc<Deps> {
        Arc::new(self.deps.clone())
    }

    pub fn validate(&self, config: &WorkerConfig) -> Result<(), RegistryError> {
        let mut errors: Vec<String> = self
            .duplicates
            .iter()
            .map(|k| format!("kind {k} registered twice"))
            .collect();
        let mut kinds: Vec<(&&str, &KindEntry)> = self.kinds.iter().collect();
        kinds.sort_by_key(|(k, _)| **k);
        for (kind, entry) in kinds {
            if let Err(e) = entry.options.validate() {
                errors.push(format!("{kind}: {e}"));
            }
            let three_beats = config.heartbeat.saturating_mul(3);
            if entry.options.lease < three_beats {
                errors.push(format!(
                    "{kind}: lease {:?} is shorter than 3 heartbeats ({three_beats:?})",
                    entry.options.lease
                ));
            }
        }
        // A silent process is pruned, and its jobs freed at once, after `prune_after`. A
        // worker that lost the database still runs a handler until one beat before the
        // shortest lease ends (spec §8.2), so pruning earlier could run a job twice.
        if let (true, Some(lease)) = (config.maintenance.enabled, self.min_lease()) {
            let give_up_after = lease.saturating_sub(config.heartbeat);
            if config.maintenance.prune_after < give_up_after {
                errors.push(format!(
                    "prune_after {:?} is shorter than the shortest lease minus a heartbeat ({give_up_after:?})",
                    config.maintenance.prune_after
                ));
            }
        }
        for r in &self.recurring {
            if r.every < Duration::from_secs(1) {
                errors.push(format!(
                    "recurring task {}: every must be at least 1 s",
                    r.task
                ));
            }
            if !self.kinds.contains_key(r.kind) {
                errors.push(format!(
                    "recurring task {}: kind {} is not registered",
                    r.task, r.kind
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(RegistryError(errors))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::QueueConfig;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize)]
    struct A;
    impl Job for A {
        const KIND: &'static str = "a.v1";
        const QUEUE: &'static str = "q";
        async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
            Ok(())
        }
    }

    #[test]
    fn validation_reports_duplicates_and_short_leases() {
        let config = WorkerConfig::new(QueueConfig::parse_list("q:1").unwrap_or_default());
        assert!(Registry::new().register::<A>().validate(&config).is_ok());
        let err = Registry::new()
            .register::<A>()
            .register::<A>()
            .validate(&config)
            .unwrap_err();
        assert!(err.0[0].contains("registered twice"));
        let mut slow = config.clone();
        slow.heartbeat = Duration::from_secs(30);
        assert!(Registry::new().register::<A>().validate(&slow).is_err());
    }

    #[test]
    fn prune_after_must_outlast_the_shortest_lease_minus_a_heartbeat() {
        // A: lease 60 s, heartbeat 20 s: handlers may run 40 s after a lost database.
        let mut config = WorkerConfig::new(QueueConfig::parse_list("q:1").unwrap_or_default());
        config.maintenance.prune_after = Duration::from_secs(60); // at least 40 s
        assert!(Registry::new().register::<A>().validate(&config).is_ok());
        config.heartbeat = Duration::from_secs(5);
        config.maintenance.prune_after = Duration::from_secs(54); // under 60 - 5
        let err = Registry::new()
            .register::<A>()
            .validate(&config)
            .unwrap_err();
        assert!(err.0[0].contains("prune_after"), "{err}");
        config.maintenance.prune_after = Duration::from_secs(55);
        assert!(Registry::new().register::<A>().validate(&config).is_ok());
        // Not checked while this process takes no part in maintenance.
        config.maintenance.prune_after = Duration::from_secs(15);
        config.maintenance.enabled = false;
        assert!(Registry::new().register::<A>().validate(&config).is_ok());
    }

    #[test]
    fn a_huge_heartbeat_is_reported_not_a_panic() {
        let mut config = WorkerConfig::new(QueueConfig::parse_list("q:1").unwrap_or_default());
        config.heartbeat = Duration::MAX;
        assert!(Registry::new().register::<A>().validate(&config).is_err());
    }
}

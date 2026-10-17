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
            if entry.options.lease < config.heartbeat * 3 {
                errors.push(format!(
                    "{kind}: lease {:?} is shorter than 3 heartbeats ({:?})",
                    entry.options.lease,
                    config.heartbeat * 3
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
}

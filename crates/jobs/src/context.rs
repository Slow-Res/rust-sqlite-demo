//! What a handler gets besides its payload (spec §5, §10).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use sqlx::PgPool;
use tokio_util::sync::{CancellationToken, WaitForCancellationFuture};

use crate::job::JobError;
use crate::rate_limit::RateLimiter;

/// A type map of shared dependencies (an SMTP transport, an HTTP client),
/// provided once to the registry and read by handlers with [`JobContext::require`].
#[derive(Default, Clone)]
pub struct Deps {
    map: HashMap<TypeId, Arc<dyn Any + Send + Sync>>,
}

impl Deps {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert<T: Send + Sync + 'static>(&mut self, value: Arc<T>) {
        self.map.insert(TypeId::of::<T>(), value);
    }

    pub fn get<T: Send + Sync + 'static>(&self) -> Option<Arc<T>> {
        self.map
            .get(&TypeId::of::<T>())
            .cloned()
            .and_then(|v| v.downcast::<T>().ok())
    }
}

#[derive(Clone)]
pub struct JobContext {
    pool: PgPool,
    id: i64,
    attempt: i32,
    kind: Arc<str>,
    token: CancellationToken,
    progress: Arc<Mutex<Option<Value>>>,
    limiter: Arc<RateLimiter>,
    deps: Arc<Deps>,
}

impl JobContext {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        pool: PgPool,
        id: i64,
        attempt: i32,
        kind: Arc<str>,
        token: CancellationToken,
        progress: Arc<Mutex<Option<Value>>>,
        limiter: Arc<RateLimiter>,
        deps: Arc<Deps>,
    ) -> Self {
        Self {
            pool,
            id,
            attempt,
            kind,
            token,
            progress,
            limiter,
            deps,
        }
    }

    /// A context outside any worker, to call a handler directly (tests, one-off
    /// scripts). Nothing it reports is written anywhere.
    pub fn detached(pool: PgPool, id: i64, attempt: i32, kind: &str, deps: Deps) -> Self {
        Self::new(
            pool,
            id,
            attempt,
            Arc::from(kind),
            CancellationToken::new(),
            Arc::new(Mutex::new(None)),
            Arc::new(RateLimiter::new()),
            Arc::new(deps),
        )
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn id(&self) -> i64 {
        self.id
    }

    /// 1 on the first run.
    pub fn attempt(&self) -> i32 {
        self.attempt
    }

    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// `"<kind>:<job id>"`: the same on every attempt, for remote de-duplication.
    pub fn idempotency_key(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }

    /// Fires when an operator cancels the job, the lease is lost, or the worker shuts down.
    pub fn cancelled(&self) -> WaitForCancellationFuture<'_> {
        self.token.cancelled()
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Report progress; the worker writes the latest value at most once per second (spec §10).
    pub fn set_progress(&self, value: Value) {
        *self.progress.lock().unwrap_or_else(PoisonError::into_inner) = Some(value);
    }

    /// Wait for a token of this process's bucket `key` (spec §12.3).
    pub async fn rate_limit(&self, key: &str, per_second: f64) {
        self.limiter.acquire(key, per_second).await;
    }

    /// A dependency provided to the registry, or a permanent error naming it.
    pub fn require<T: Send + Sync + 'static>(&self) -> Result<Arc<T>, JobError> {
        self.deps.get::<T>().ok_or_else(|| {
            JobError::Permanent(format!("missing dependency {}", std::any::type_name::<T>()))
        })
    }
}

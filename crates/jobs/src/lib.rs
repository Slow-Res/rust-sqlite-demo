//! Background jobs on Postgres: a transactional outbox plus a job queue with
//! per-job leases, a process registry and an elected maintenance leader.
//! Design: docs/superpowers/specs/2026-10-14-background-jobs-design.md.
//!
//! The app enqueues inside its own transactions ([`Enqueuer`]) and runs
//! [`Worker`]s in a separate process. Delivery is at least once: handlers
//! must be idempotent.

use sqlx::PgPool;

pub mod backoff;
pub mod config;
pub mod context;
pub mod enqueue;
pub mod executor;
pub mod heartbeat;
#[cfg(feature = "http")]
pub mod http;
pub mod job;
pub mod maintenance;
pub mod metrics;
pub mod ops;
pub mod rate_limit;
pub mod ready;
pub mod registry;
pub mod running;
pub mod store;
pub mod worker;

pub use backoff::Backoff;
pub use config::{ConfigError, MaintenanceConfig, QueueConfig, WorkerConfig};
pub use context::{Deps, JobContext};
pub use enqueue::{
    EnqueueError, EnqueueOptions, Enqueued, Enqueuer, MAX_ARGS_BYTES, NOTIFY_CHANNEL,
};
pub use heartbeat::Health;
pub use job::{Job, JobError, JobOptions};
pub use metrics::Metrics;
pub use rate_limit::RateLimiter;
pub use ready::{NotReady, ready};
pub use registry::{Registry, RegistryError};
pub use worker::{ExitReason, Worker, WorkerError};

/// The crate's own migration history table, so its versions never mix with the
/// app's `_sqlx_migrations` (spec §3.3).
pub const MIGRATIONS_TABLE: &str = "_jobs_migrations";

/// Create or update the jobs schema. Idempotent and safe to run from every
/// process at start-up (sqlx serialises concurrent runs with an advisory lock).
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    let mut migrator = sqlx::migrate!("./migrations");
    migrator.dangerous_set_table_name(MIGRATIONS_TABLE);
    migrator.run(pool).await
}

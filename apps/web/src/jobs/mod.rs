//! The library app's background jobs (docs/JOBS.md): the kinds it defines, where
//! they are enqueued, and the `worker` and `jobs` subcommands. The queue itself is
//! the generic `library-jobs` crate.

pub mod events;
pub mod loan_email;
pub mod ops_cli;
pub mod worker;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use library_jobs::{QueueConfig, Registry, WorkerConfig};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::jobs_config::JobsConfig;

/// Every kind this process can run, with what the handlers need. A kind whose
/// settings are missing (no SMTP URL, no webhook endpoint) is left out, so its
/// jobs wait, and `jobs status` names them, instead of failing.
pub fn registry(jobs: &JobsConfig) -> anyhow::Result<Registry> {
    let mut registry = Registry::new();
    if let Some(mailer) = loan_email::Mailer::from_config(jobs)? {
        registry = registry
            .register::<loan_email::LoanEmail>()
            .provide(Arc::new(mailer));
    }
    // The webhook kind joins in Task 18.
    Ok(registry)
}

pub fn worker_config(jobs: &JobsConfig, process_kind: &str) -> anyhow::Result<WorkerConfig> {
    let queues = QueueConfig::parse_list(&jobs.queues).context("JOBS_QUEUES")?;
    let mut config = WorkerConfig::new(queues);
    config.poll = Duration::from_millis(jobs.poll_ms);
    config.listen = jobs.notify;
    config.shutdown_timeout = Duration::from_secs(jobs.shutdown_timeout_s);
    config.process_kind = process_kind.to_owned();
    config.version = env!("CARGO_PKG_VERSION").to_owned();
    config.maintenance.keep_succeeded = Duration::from_secs(jobs.keep_succeeded_h * 3600);
    config.maintenance.keep_failed = Duration::from_secs(jobs.keep_failed_h * 3600);
    config.validate()?;
    Ok(config)
}

/// The jobs schema, after the app's (spec §3.3).
pub async fn migrate(pool: &PgPool) -> anyhow::Result<()> {
    library_jobs::migrate(pool).await.context("jobs migrations")
}

/// Longest a single statement of a worker connection may run (spec §11.4): a
/// stuck query must not hold a claim or a lease write forever.
const STATEMENT_TIMEOUT: &str = "30s";

/// A pool of its own, so job work never takes the API's connections. Every
/// connection gets `statement_timeout`; the app's own pool is left alone.
pub async fn connect(database_url: &str, max_connections: u32) -> anyhow::Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SET statement_timeout = '{STATEMENT_TIMEOUT}'"
                )))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect(database_url)
        .await
        .context("connecting to DATABASE_URL")
}

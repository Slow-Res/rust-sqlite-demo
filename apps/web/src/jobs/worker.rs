//! `library-web worker`, and the worker embedded in `serve` (spec §3.2).

use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use library_jobs::{ExitReason, Worker};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::db::{self, Database};

/// The metrics server drains without a bound of its own (a scraper holding a
/// connection open would stall it), so the process gives it this long.
const HTTP_DRAIN: Duration = Duration::from_secs(5);

pub async fn run(config: Config) -> anyhow::Result<()> {
    if !config.jobs.enabled {
        bail!("JOBS_ENABLED is not true: the worker has nothing to do");
    }
    let pool = super::connect(&config.database_url, config.jobs.db_pool).await?;
    db::migrate(&Database::Postgres(pool.clone())).await?;
    super::migrate(&pool).await?;
    let worker = Worker::new(
        pool.clone(),
        super::registry(&config.jobs)?,
        super::worker_config(&config.jobs, "worker")?,
    )?;

    let stop = CancellationToken::new();
    let router = worker.router();
    let addr = config.jobs.metrics_addr;
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding JOBS_METRICS_ADDR {addr}"))?;
    tracing::info!(%addr, "worker metrics and health listening");
    let mut http = tokio::spawn(library_jobs::http::serve(listener, router, stop.clone()));
    let signal = {
        let stop = stop.clone();
        tokio::spawn(async move {
            crate::server::shutdown_signal().await;
            stop.cancel();
        })
    };

    let reason = worker.run(stop.clone()).await;
    stop.cancel();
    signal.abort();
    if tokio::time::timeout(HTTP_DRAIN, &mut http).await.is_err() {
        tracing::warn!("the metrics server did not drain in {HTTP_DRAIN:?}; aborting it");
        http.abort();
    }
    pool.close().await;
    match reason? {
        ExitReason::Shutdown => Ok(()),
        ExitReason::SelfFenced => {
            bail!(
                "the worker fenced itself off (its registry row was deleted); exiting so the supervisor restarts it"
            )
        }
    }
}

/// A worker running inside `serve` (`JOBS_EMBEDDED=true`).
pub struct Embedded {
    stop: CancellationToken,
    /// Cancelled when the worker task ends for any reason, a panic included.
    ended: CancellationToken,
    handle: JoinHandle<anyhow::Result<()>>,
}

impl Embedded {
    /// Fires when the worker ended without being asked to. `serve` begins its
    /// graceful shutdown then, and exits non-zero so the supervisor restarts the
    /// process (spec §8.2): a fenced or failed worker must not leave a server
    /// that quietly stopped running jobs.
    pub fn ended(&self) -> CancellationToken {
        self.ended.clone()
    }

    /// Graceful stop: finish or release running jobs (spec §13). `Err` only when
    /// the worker had already failed on its own; a requested stop is `Ok`.
    pub async fn stop(self) -> anyhow::Result<()> {
        self.stop.cancel();
        match self.handle.await {
            Ok(result) => result,
            Err(err) => Err(anyhow!("the embedded jobs worker task failed: {err}")),
        }
    }
}

/// What `serve` does about jobs: nothing when disabled; otherwise migrate the jobs
/// schema and, if embedded, start a worker on a pool of its own.
pub async fn on_serve(database: &Database, config: &Config) -> anyhow::Result<Option<Embedded>> {
    if !config.jobs.enabled {
        return Ok(None);
    }
    let Database::Postgres(pool) = database else {
        bail!("JOBS_ENABLED=true needs a Postgres DATABASE_URL");
    };
    super::migrate(pool).await?;
    if !config.jobs.embedded {
        return Ok(None);
    }
    let pool = super::connect(&config.database_url, config.jobs.db_pool).await?;
    let worker = Worker::new(
        pool.clone(),
        super::registry(&config.jobs)?,
        super::worker_config(&config.jobs, "embedded")?,
    )?;
    let stop = CancellationToken::new();
    let ended = CancellationToken::new();
    let handle = {
        let stop = stop.clone();
        let ended = ended.clone();
        tokio::spawn(async move {
            // Dropped on every exit path, so a panic also wakes `serve`.
            let _ended = ended.drop_guard();
            let result = match worker.run(stop).await {
                Ok(ExitReason::Shutdown) => Ok(()),
                Ok(ExitReason::SelfFenced) => Err(anyhow!(
                    "the embedded jobs worker fenced itself off (its registry row was deleted)"
                )),
                Err(err) => Err(anyhow::Error::new(err).context("the embedded jobs worker failed")),
            };
            pool.close().await;
            result
        })
    };
    tracing::info!("embedded jobs worker started");
    Ok(Some(Embedded {
        stop,
        ended,
        handle,
    }))
}

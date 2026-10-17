//! Binding the listener and running the server until a shutdown signal.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use tokio::net::TcpListener;

use crate::clock::SystemClock;
use crate::config::Config;
use crate::state::AppState;
use crate::{build_router, checkpoint, db};

/// Run the HTTP server: connect, migrate, serve, and shut down gracefully.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let database = db::connect(&config).await?;
    db::migrate(&database).await?;
    #[cfg(feature = "postgres")]
    let embedded_jobs = crate::jobs::worker::on_serve(&database, &config).await?;

    // SQLite only; Postgres checkpoints itself.
    let checkpoints = match &database {
        db::Database::Sqlite(pool) if config.sqlite_checkpoint_interval_ms > 0 => {
            let every = Duration::from_millis(config.sqlite_checkpoint_interval_ms);
            tracing::info!(
                every_ms = config.sqlite_checkpoint_interval_ms,
                "WAL checkpoint task on"
            );
            Some(checkpoint::spawn(pool.clone(), every))
        }
        _ => None,
    };

    let bind_addr = config.bind_addr;
    let state = AppState::new(database.clone(), Arc::new(config), Arc::new(SystemClock));
    let router = build_router(state);

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "listening");

    // An embedded worker that ends on its own (fenced, failed) also ends the
    // server; the process then exits non-zero (spec §8.2).
    #[cfg(feature = "postgres")]
    let worker_ended = embedded_jobs.as_ref().map(|jobs| jobs.ended());
    let stop_serving = async move {
        #[cfg(feature = "postgres")]
        if let Some(ended) = worker_ended {
            tokio::select! {
                () = shutdown_signal() => {},
                () = ended.cancelled() => tracing::error!("the embedded jobs worker ended; shutting down"),
            }
            return;
        }
        shutdown_signal().await;
    };
    let served = axum::serve(listener, router)
        .with_graceful_shutdown(stop_serving)
        .await
        .context("server error");

    if let Some(task) = checkpoints {
        task.abort();
    }
    #[cfg(feature = "postgres")]
    let jobs_outcome = match embedded_jobs {
        Some(jobs) => jobs.stop().await,
        None => Ok(()),
    };
    #[cfg(not(feature = "postgres"))]
    let jobs_outcome: anyhow::Result<()> = Ok(());
    database.close().await;
    served?;
    jobs_outcome?;
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Resolves on Ctrl-C or (on Unix) SIGTERM, which is what container runtimes send.
pub(crate) async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::error!(error = ?err, "failed to listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
            }
            Err(err) => {
                tracing::error!(error = ?err, "failed to listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    // `select!` finishes as soon as the first of the two futures does.
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received");
}

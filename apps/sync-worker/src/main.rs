//! Thin entry point: load `.env`, parse the CLI, set up logging, dispatch.

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use sync_worker::cli::{Cli, Command};
use sync_worker::config::Config;
use sync_worker::metrics::Metrics;
use sync_worker::run::{self, Settings};
use sync_worker::{http, status, telemetry};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env file is normal (containers use real env vars).
    dotenvy::dotenv().ok();
    let command = Cli::parse().command();
    let config = Config::from_env()?;
    telemetry::init(config.log_format);
    match command {
        Command::Run => run_worker(config).await,
        Command::Status => {
            print!("{}", status::status(&config).await?);
            Ok(())
        }
    }
}

async fn run_worker(config: Config) -> anyhow::Result<()> {
    let metrics = Arc::new(Metrics::new());
    let shutdown = CancellationToken::new();
    tokio::spawn(cancel_on_signal(shutdown.clone()));
    let listener = tokio::net::TcpListener::bind(config.metrics_addr)
        .await
        .with_context(|| format!("binding {}", config.metrics_addr))?;
    tracing::info!(node = %config.node_id, metrics = %config.metrics_addr, "sync-worker started");
    let server = tokio::spawn(http::serve(listener, metrics.clone(), shutdown.clone()));
    run::run(
        &config,
        &Settings::production(config.batch_size),
        &metrics,
        &shutdown,
    )
    .await?;
    shutdown.cancel();
    server.await.context("metrics server task")??;
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Cancels `shutdown` on Ctrl-C or SIGTERM (what container runtimes send).
async fn cancel_on_signal(shutdown: CancellationToken) {
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
    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutdown signal received; finishing the current cycle");
    shutdown.cancel();
}

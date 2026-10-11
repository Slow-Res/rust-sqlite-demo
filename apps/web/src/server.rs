//! Binding the listener and running the server until a shutdown signal.

use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;

use crate::clock::SystemClock;
use crate::config::Config;
use crate::state::AppState;
use crate::{build_router, db};

/// Run the HTTP server: connect, migrate, serve, and shut down gracefully.
pub async fn serve(config: Config) -> anyhow::Result<()> {
    let pool = db::connect(&config).await?;
    db::migrate(&pool).await?;

    let bind_addr = config.bind_addr;
    let state = AppState::new(pool.clone(), Arc::new(config), Arc::new(SystemClock));
    let router = build_router(state);

    let listener = TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("binding {bind_addr}"))?;
    tracing::info!(%bind_addr, "listening");

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    pool.close().await;
    tracing::info!("shut down cleanly");
    Ok(())
}

/// Resolves on Ctrl-C or (on Unix) SIGTERM, which is what container runtimes send.
async fn shutdown_signal() {
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

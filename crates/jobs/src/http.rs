//! `/metrics`, `/healthz`, `/readyz` as an axum router (feature `http`, spec
//! §17.3). Serve it on an internal address, or behind admin auth.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::heartbeat::Health;
use crate::metrics::Metrics;
use crate::ready::ready;

#[derive(Clone)]
struct HttpState {
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    pool: PgPool,
    stale_after: Duration,
}

/// The three endpoints. `/readyz` treats a heartbeat older than three
/// `heartbeat` periods as stale.
pub fn router(
    metrics: Arc<Metrics>,
    health: Arc<Health>,
    pool: PgPool,
    heartbeat: Duration,
) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok\n" }))
        .route("/readyz", get(readyz))
        .route("/metrics", get(metrics_text))
        .with_state(HttpState {
            metrics,
            health,
            pool,
            stale_after: heartbeat * 3,
        })
}

async fn readyz(State(s): State<HttpState>) -> (StatusCode, String) {
    match ready(&s.health, &s.pool, s.stale_after).await {
        Ok(()) => (StatusCode::OK, "ready\n".into()),
        Err(not_ready) => (StatusCode::SERVICE_UNAVAILABLE, format!("{not_ready}\n")),
    }
}

async fn metrics_text(State(s): State<HttpState>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        s.metrics.encode(),
    )
}

/// Serve `router` on `listener` until `stop` is cancelled.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    stop: CancellationToken,
) -> std::io::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { stop.cancelled().await })
        .await
}

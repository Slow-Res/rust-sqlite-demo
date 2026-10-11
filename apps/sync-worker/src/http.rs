//! `/health` and `/metrics` (spec §8). Reachable only inside the Docker
//! network: node.yml never publishes this port.

use std::sync::Arc;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::metrics::{Metrics, now_secs};

/// Healthy means a cycle succeeded within this many seconds.
const HEALTHY_WITHIN_SECS: f64 = 60.0;

pub fn router(metrics: Arc<Metrics>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/metrics", get(metrics_text))
        .with_state(metrics)
}

async fn health(State(metrics): State<Arc<Metrics>>) -> (StatusCode, &'static str) {
    if metrics.healthy(now_secs(), HEALTHY_WITHIN_SECS) {
        (StatusCode::OK, "ok\n")
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "no successful sync cycle in the last 60 s\n",
        )
    }
}

async fn metrics_text(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/openmetrics-text; version=1.0.0; charset=utf-8",
        )],
        metrics.encode(),
    )
}

/// Serve until `shutdown` is cancelled.
pub async fn serve(
    listener: TcpListener,
    metrics: Arc<Metrics>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    axum::serve(listener, router(metrics))
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .context("metrics server")
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn get(router: Router, path: &str) -> (StatusCode, String) {
        let response = router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn health_is_503_until_a_cycle_succeeds() {
        let metrics = Arc::new(Metrics::new());
        assert_eq!(
            get(router(metrics.clone()), "/health").await.0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        metrics.last_success_seconds.set(now_secs());
        assert_eq!(get(router(metrics), "/health").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn metrics_endpoint_serves_the_registry() {
        let (status, body) = get(router(Arc::new(Metrics::new())), "/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("sync_lag_seconds"), "{body}");
    }
}

//! `GET /api/health`: liveness plus a database round-trip.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use library_shared::Health;
use sqlx::SqlitePool;

use crate::config::Config;
use crate::extract::ApiJson;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(
    State(pool): State<SqlitePool>,
    State(config): State<Arc<Config>>,
) -> (StatusCode, ApiJson<Health>) {
    let health = |status: &str| Health {
        status: status.to_owned(),
        node_id: config.node_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (StatusCode::OK, ApiJson(health("ok"))),
        Err(err) => {
            tracing::error!(error = ?err, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ApiJson(health("unavailable")),
            )
        }
    }
}

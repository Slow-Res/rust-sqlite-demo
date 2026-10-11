//! `GET /api/health`: liveness plus a database round-trip.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use library_shared::Health;

use crate::config::Config;
use crate::db::Database;
use crate::extract::ApiJson;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(
    State(db): State<Database>,
    State(config): State<Arc<Config>>,
) -> (StatusCode, ApiJson<Health>) {
    let health = |status: &str| Health {
        status: status.to_owned(),
        node_id: config.node_id.clone(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
    };
    match db.ping().await {
        Ok(()) => (StatusCode::OK, ApiJson(health("ok"))),
        Err(err) => {
            tracing::error!(error = ?err, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ApiJson(health("unavailable")),
            )
        }
    }
}

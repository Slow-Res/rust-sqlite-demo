//! `GET /api/health`: liveness plus a database round-trip.

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use library_shared::Health;
use sqlx::SqlitePool;

use crate::extract::ApiJson;
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new().route("/health", get(health))
}

async fn health(State(pool): State<SqlitePool>) -> (StatusCode, ApiJson<Health>) {
    match sqlx::query("SELECT 1").execute(&pool).await {
        Ok(_) => (
            StatusCode::OK,
            ApiJson(Health {
                status: "ok".to_owned(),
            }),
        ),
        Err(err) => {
            tracing::error!(error = ?err, "health check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                ApiJson(Health {
                    status: "unavailable".to_owned(),
                }),
            )
        }
    }
}

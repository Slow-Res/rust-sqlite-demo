//! Integration tests for the foundation routes.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use sqlx::SqlitePool;

// `#[sqlx::test(...)]` creates a fresh database, runs `migrations/sqlite/`, and passes the pool in.
#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn health_is_ok(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, headers, body) = app.get("/api/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({ "status": "ok" }));
    assert!(headers.contains_key("x-request-id"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn unknown_api_route_is_json_404(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, _, body) = app.get("/api/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}

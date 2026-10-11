//! Integration tests for the foundation routes.

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;

// `db_tests!` runs each body on a fresh, migrated SQLite database and, with the
// `postgres` feature, on a fresh Postgres one (see `common/mod.rs`).
db_tests! {
async fn health_is_ok(db: Database) {
    let app = TestApp::new(db);
    let (status, headers, body) = app.get("/api/health", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({ "status": "ok", "node_id": "test-node", "version": env!("CARGO_PKG_VERSION") })
    );
    assert!(headers.contains_key("x-request-id"));
    assert_eq!(headers["x-content-type-options"], "nosniff");
}

async fn unknown_api_route_is_json_404(db: Database) {
    let app = TestApp::new(db);
    let (status, _, body) = app.get("/api/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
}
}

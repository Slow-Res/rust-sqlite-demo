//! Integration tests for the router's middleware stack and the SPA fallback.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::TestApp;
use http_body_util::BodyExt;
use sqlx::SqlitePool;
use tower::ServiceExt;

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn security_headers_are_present(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (_, headers, _) = app.get("/api/health", None).await;
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "same-origin");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn client_request_id_is_propagated_unchanged(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let request = Request::builder()
        .uri("/api/health")
        .header("x-request-id", "req-from-client-123")
        .body(Body::empty())
        .unwrap();
    let response = app.router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.headers()["x-request-id"], "req-from-client-123");
}

fn oversized_login(content_length: Option<usize>) -> Request<Body> {
    let body = vec![b'a'; 64 * 1024 + 1];
    let mut builder = Request::builder()
        .method(Method::POST)
        .uri("/api/auth/login")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(len) = content_length {
        builder = builder.header(header::CONTENT_LENGTH, len);
    }
    builder.body(Body::from(body)).unwrap()
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn declared_body_over_64_kib_is_rejected_with_413(pool: SqlitePool) {
    let app = TestApp::new(pool);
    // Real clients announce the size, so the limit layer answers before any extractor runs.
    let response = app
        .router
        .clone()
        .oneshot(oversized_login(Some(64 * 1024 + 1)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], "payload_too_large");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn wrong_method_is_a_json_405(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, headers, json) = app.delete("/api/health", None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(headers[header::CONTENT_TYPE], "application/json");
    assert!(headers.contains_key(header::ALLOW));
    assert_eq!(json["error"]["code"], "method_not_allowed");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn undeclared_body_over_64_kib_is_rejected_with_400(pool: SqlitePool) {
    let app = TestApp::new(pool);
    // Without a `Content-Length` (e.g. chunked) the limit trips while the JSON
    // extractor reads the body, which maps to 400 like any extractor rejection.
    let response = app
        .router
        .clone()
        .oneshot(oversized_login(None))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn spa_fallback_serves_index_but_api_stays_json(pool: SqlitePool) {
    let dir = std::env::temp_dir().join(format!("library-spa-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<!doctype html><title>spa</title>").unwrap();
    let app = TestApp::with_frontend_dir(pool, dir.clone());

    for uri in ["/", "/books/1"] {
        let (status, _, body) = app.send_text(Method::GET, uri, Body::empty()).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(body.contains("<title>spa</title>"), "{uri}: {body}");
    }

    let (status, _, body) = app.get("/api/nope", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");

    std::fs::remove_dir_all(&dir).ok();
}

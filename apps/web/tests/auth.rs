//! Integration tests for `/api/auth/*` and the `AuthUser` extractor.

mod common;

use axum::http::{StatusCode, header};
use chrono::Duration;
use common::TestApp;
use serde_json::json;
use sqlx::SqlitePool;

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn login_success_returns_user_and_cookie(pool: SqlitePool) {
    let app = TestApp::new(pool);
    app.auth.create_user("ada", "password123").await.unwrap();

    let body = json!({ "username": "ada", "password": "password123" });
    let (status, headers, json) = app.post_json("/api/auth/login", &body, None).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["username"], "ada");
    assert!(json["id"].is_i64());
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.starts_with("library_session="), "{cookie}");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Lax"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(cookie.contains("Max-Age=604800"), "{cookie}");
    assert!(!cookie.contains("; Secure"), "{cookie}");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn secure_config_marks_the_cookie_secure(pool: SqlitePool) {
    let app = TestApp::with_secure_cookie(pool);
    app.auth.create_user("ada", "password123").await.unwrap();

    let body = json!({ "username": "ada", "password": "password123" });
    let (status, headers, _) = app.post_json("/api/auth/login", &body, None).await;

    assert_eq!(status, StatusCode::OK);
    let cookie = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(cookie.contains("; Secure"), "{cookie}");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn wrong_password_and_unknown_user_look_identical(pool: SqlitePool) {
    let app = TestApp::new(pool);
    app.auth.create_user("ada", "password123").await.unwrap();

    let wrong = json!({ "username": "ada", "password": "nope-nope" });
    let unknown = json!({ "username": "ghost", "password": "nope-nope" });
    let (s1, _, b1) = app.post_json("/api/auth/login", &wrong, None).await;
    let (s2, _, b2) = app.post_json("/api/auth/login", &unknown, None).await;

    assert_eq!(s1, StatusCode::UNAUTHORIZED);
    assert_eq!(b1["error"]["code"], "unauthorized");
    assert_eq!(b1["error"]["message"], "invalid username or password");
    assert_eq!((s1, b1), (s2, b2));
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn login_ignores_username_case(pool: SqlitePool) {
    let app = TestApp::new(pool);
    app.auth.create_user("Ada", "password123").await.unwrap();
    let body = json!({ "username": "ADA", "password": "password123" });
    let (status, _, _) = app.post_json("/api/auth/login", &body, None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn empty_fields_are_422(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let body = json!({ "username": "", "password": "" });
    let (status, _, json) = app.post_json("/api/auth/login", &body, None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["code"], "validation_error");
    assert!(json["error"]["fields"]["username"].is_string());
    assert!(json["error"]["fields"]["password"].is_string());
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn malformed_json_is_400(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, _, json) = app
        .send_raw(
            axum::http::Method::POST,
            "/api/auth/login",
            Some("{ not json".to_owned()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["code"], "bad_request");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn me_requires_a_valid_session(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;

    let (status, _, json) = app.get("/api/auth/me", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["username"], "ada");

    let (status, _, json) = app.get("/api/auth/me", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "unauthorized");

    let garbage = "library_session=not-a-real-token";
    let (status, _, _) = app.get("/api/auth/me", Some(garbage)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn expired_session_is_rejected(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;

    app.clock.advance(Duration::hours(167));
    let (status, _, _) = app.get("/api/auth/me", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);

    app.clock.advance(Duration::hours(2));
    let (status, _, _) = app.get("/api/auth/me", Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn login_sweeps_expired_sessions(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    app.login("ada", "password123").await;
    app.clock.advance(Duration::hours(200));

    let body = json!({ "username": "ada", "password": "password123" });
    app.post_json("/api/auth/login", &body, None).await;

    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn session_table_stores_only_a_hash(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let token = cookie.strip_prefix("library_session=").unwrap();

    let stored: String = sqlx::query_scalar("SELECT token_hash FROM sessions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_ne!(stored, token);
    assert_eq!(stored.len(), 64);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn logout_ends_the_session(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;

    let (status, headers, _) = app
        .post_json("/api/auth/logout", &json!({}), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let removal = headers[header::SET_COOKIE].to_str().unwrap();
    assert!(removal.starts_with("library_session=;"), "{removal}");
    assert!(removal.contains("Max-Age=0"), "{removal}");

    let (status, _, _) = app.get("/api/auth/me", Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn logout_without_session_is_401(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, _, _) = app.post_json("/api/auth/logout", &json!({}), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn usernames_are_unique_ignoring_case(pool: SqlitePool) {
    let app = TestApp::new(pool);
    app.auth.create_user("Ada", "password123").await.unwrap();
    let err = app
        .auth
        .create_user("ada", "password123")
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        library_web::error::AppError::Conflict {
            code: "username_taken",
            ..
        }
    ));
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn create_user_validates_input(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let err = app.auth.create_user("  ", "short").await.unwrap_err();
    match err {
        library_web::error::AppError::Validation(fields) => {
            let names: Vec<_> = fields.iter().map(|(f, _)| f).collect();
            assert_eq!(names, ["password", "username"]);
        }
        other => panic!("expected Validation, got {other:?}"),
    }
}

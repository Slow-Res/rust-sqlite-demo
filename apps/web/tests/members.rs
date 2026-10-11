//! Integration tests for `/api/members`.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::{TestApp, insert_loan};
use serde_json::{Value, json};
use sqlx::SqlitePool;

fn member(name: &str, email: &str) -> Value {
    json!({ "name": name, "email": email })
}

async fn create(app: &TestApp, cookie: &str, body: Value) -> Value {
    let (status, _, json) = app.post_json("/api/members", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json
}

fn names(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn every_endpoint_requires_auth(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let body = member("X", "x@example.com");
    let (s, _, _) = app.get("/api/members", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.get("/api/members/1", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.post_json("/api/members", &body, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.put_json("/api/members/1", &body, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.delete("/api/members/1", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn create_trims_and_lowercases(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, member("  Grace  ", " Grace@Example.COM ")).await;
    assert_eq!(created["name"], "Grace");
    assert_eq!(created["email"], "grace@example.com");
    assert_eq!(created["active_loans"], 0);
    assert!(created["created_at"].is_string());

    let id = created["id"].as_i64().unwrap();
    let (status, _, one) = app.get(&format!("/api/members/{id}"), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one, created);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn invalid_fields_are_all_reported(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let (status, _, json) = app
        .post_json("/api/members", &member(" ", "nope"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["code"], "validation_error");
    for field in ["name", "email"] {
        assert!(
            json["error"]["fields"][field].is_string(),
            "{field}: {json}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn duplicate_email_is_409_case_insensitively(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, member("A", "a@example.com")).await;
    let (status, _, json) = app
        .post_json("/api/members", &member("B", "A@EXAMPLE.com"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "email_taken");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn get_missing_is_404(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let (status, _, json) = app.get("/api/members/999", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["message"], "member not found");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn list_searches_name_and_email_ordered_by_name(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, member("Charlie", "c@example.com")).await;
    create(&app, &cookie, member("Alice", "alice@work.org")).await;
    create(&app, &cookie, member("Bob", "bob@example.com")).await;

    let (_, _, all) = app.get("/api/members", Some(&cookie)).await;
    assert_eq!(names(&all), ["Alice", "Bob", "Charlie"]);
    assert_eq!(all["total"], 3);

    let (_, _, by_name) = app.get("/api/members?q=%20ALI%20", Some(&cookie)).await;
    assert_eq!(names(&by_name), ["Alice"]);
    let (_, _, by_email) = app.get("/api/members?q=example.com", Some(&cookie)).await;
    assert_eq!(names(&by_email), ["Bob", "Charlie"]);
    // `%` and `_` are literal, not wildcards.
    let (_, _, none) = app.get("/api/members?q=%25", Some(&cookie)).await;
    assert_eq!(none["total"], 0);
    let (_, _, none) = app.get("/api/members?q=_", Some(&cookie)).await;
    assert_eq!(none["total"], 0);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn search_longer_than_200_chars_is_rejected(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let uri = format!("/api/members?q={}", "a".repeat(201));
    let (status, _, body) = app.get(&uri, Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");
    assert_eq!(
        body["error"]["fields"]["q"],
        "must be at most 200 characters"
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn list_paginates(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    for (name, email) in [("A", "a@x.io"), ("B", "b@x.io"), ("C", "c@x.io")] {
        create(&app, &cookie, member(name, email)).await;
    }
    let (_, _, page2) = app
        .get("/api/members?page=2&per_page=2", Some(&cookie))
        .await;
    assert_eq!(names(&page2), ["C"]);
    assert_eq!(page2["total"], 3);
    assert_eq!(page2["page"], 2);
    assert_eq!(page2["per_page"], 2);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn active_loans_counts_only_unreturned(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, member("A", "a@x.io")).await;
    let id = created["id"].as_i64().unwrap();
    insert_loan(&pool, id, false).await;
    insert_loan(&pool, id, false).await;
    insert_loan(&pool, id, true).await;

    let (_, _, one) = app.get(&format!("/api/members/{id}"), Some(&cookie)).await;
    assert_eq!(one["active_loans"], 2);
    let (_, _, page) = app.get("/api/members", Some(&cookie)).await;
    assert_eq!(page["items"][0]["active_loans"], 2);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn update_replaces_fields(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, member("A", "a@x.io")).await;
    let id = created["id"].as_i64().unwrap();
    app.clock.advance(chrono::Duration::hours(1));

    let uri = format!("/api/members/{id}");
    let (status, _, updated) = app
        .put_json(&uri, &member("New Name", "New@X.io"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(updated["name"], "New Name");
    assert_eq!(updated["email"], "new@x.io");
    assert_eq!(updated["created_at"], created["created_at"]);
    assert_ne!(updated["updated_at"], created["updated_at"]);

    // Saving the member's own email again is not a conflict.
    let (status, _, _) = app
        .put_json(&uri, &member("New Name", "new@x.io"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn update_missing_invalid_and_taken(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, member("A", "a@x.io")).await;
    let b = create(&app, &cookie, member("B", "b@x.io")).await;
    let b_uri = format!("/api/members/{}", b["id"]);

    let (status, _, _) = app
        .put_json("/api/members/999", &member("X", "x@x.io"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _, _) = app
        .put_json(&b_uri, &member("", "x@x.io"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, _, json) = app
        .put_json(&b_uri, &member("B", "A@x.io"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "email_taken");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn delete_outcomes(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;

    let (status, _, _) = app.delete("/api/members/999", Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let busy = create(&app, &cookie, member("Busy", "busy@x.io")).await;
    let busy_id = busy["id"].as_i64().unwrap();
    insert_loan(&pool, busy_id, false).await;
    let (status, _, json) = app
        .delete(&format!("/api/members/{busy_id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "member_has_active_loans");

    // Returned loans are removed together with the member.
    let done = create(&app, &cookie, member("Done", "done@x.io")).await;
    let done_id = done["id"].as_i64().unwrap();
    insert_loan(&pool, done_id, true).await;
    let (status, _, _) = app
        .delete(&format!("/api/members/{done_id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = app
        .get(&format!("/api/members/{done_id}"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let loans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM loans WHERE member_id = ?")
        .bind(done_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loans, 0);
}

//! Integration tests for `/api/books`.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::SqlitePool;

fn book(title: &str) -> Value {
    json!({ "title": title, "author": "Some Author", "isbn": null, "published_year": 2000, "total_copies": 2 })
}

async fn create(app: &TestApp, cookie: &str, body: Value) -> Value {
    let (status, _, json) = app.post_json("/api/books", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json
}

/// Insert a member and a loan straight into the database (the loans API is a
/// later feature). Returns the loan id.
async fn insert_loan(pool: &SqlitePool, book_id: i64, returned: bool) -> i64 {
    // Each loan gets its own member (emails are unique).
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM members")
        .fetch_one(pool)
        .await
        .unwrap();
    let email = format!("member{n}@example.com");
    let member_id = sqlx::query(
        "INSERT INTO members (name, email, created_at, updated_at) VALUES ('M', ?, 0, 0)",
    )
    .bind(email)
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid();
    sqlx::query(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES (?, ?, 0, 100, ?)",
    )
    .bind(book_id)
    .bind(member_id)
    .bind(returned.then_some(50))
    .execute(pool)
    .await
    .unwrap()
    .last_insert_rowid()
}

fn titles(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["title"].as_str().unwrap())
        .collect()
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn list_and_get_are_public(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, book("Dune")).await;
    let id = created["id"].as_i64().unwrap();

    let (status, _, page) = app.get("/api/books", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(titles(&page), ["Dune"]);
    assert_eq!(page["total"], 1);

    let (status, _, one) = app.get(&format!("/api/books/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["title"], "Dune");
    assert_eq!(one["available_copies"], 2);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn get_missing_is_404(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, _, json) = app.get("/api/books/999", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["code"], "not_found");
    assert_eq!(json["error"]["message"], "book not found");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn mutations_require_auth(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (s, _, _) = app.post_json("/api/books", &book("X"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.put_json("/api/books/1", &book("X"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _, _) = app.delete("/api/books/1", None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn create_normalizes_isbn(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let mut body = book("SICP");
    body["isbn"] = json!("978-0-13-110362-7");
    let created = create(&app, &cookie, body).await;
    assert_eq!(created["isbn"], "9780131103627");
    assert_eq!(created["total_copies"], 2);
    assert!(created["created_at"].is_string());
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn invalid_fields_are_all_reported(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let body = json!({ "title": " ", "author": "", "isbn": "123", "published_year": 3000, "total_copies": 0 });
    let (status, _, json) = app.post_json("/api/books", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["code"], "validation_error");
    for field in ["title", "author", "isbn", "published_year", "total_copies"] {
        assert!(
            json["error"]["fields"][field].is_string(),
            "{field}: {json}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn duplicate_isbn_is_409(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let mut body = book("A");
    body["isbn"] = json!("9780131103627");
    create(&app, &cookie, body.clone()).await;

    // Same ISBN written differently still collides after normalization.
    body["isbn"] = json!("978-0-13-110362-7");
    let (status, _, json) = app.post_json("/api/books", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "isbn_taken");

    // Books without an ISBN never collide.
    create(&app, &cookie, book("B")).await;
    create(&app, &cookie, book("C")).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn update_to_taken_isbn_is_409(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let mut a = book("A");
    a["isbn"] = json!("9780131103627");
    create(&app, &cookie, a).await;
    let b = create(&app, &cookie, book("B")).await;

    let mut body = book("B");
    body["isbn"] = json!("0-306-40615-2");
    let uri = format!("/api/books/{}", b["id"]);
    let (status, _, _) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    body["isbn"] = json!("9780131103627");
    let (status, _, json) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "isbn_taken");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn search_matches_title_author_isbn_case_insensitively(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, book("The Rust Book")).await;
    create(
        &app,
        &cookie,
        json!({ "title": "Dune", "author": "Frank Herbert", "isbn": "9780131103627", "published_year": null, "total_copies": 1 }),
    )
    .await;

    let (_, _, page) = app.get("/api/books?q=rUsT", None).await;
    assert_eq!(titles(&page), ["The Rust Book"]);
    let (_, _, page) = app.get("/api/books?q=herbert", None).await;
    assert_eq!(titles(&page), ["Dune"]);
    let (_, _, page) = app.get("/api/books?q=0131", None).await;
    assert_eq!(titles(&page), ["Dune"]);
    let (_, _, page) = app.get("/api/books?q=%20%20", None).await;
    assert_eq!(page["total"], 2);
    let (_, _, page) = app.get("/api/books?q=nothing", None).await;
    assert_eq!(page["total"], 0);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn search_matches_isbn_typed_with_hyphens_or_spaces(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, book("The Rust Book")).await;
    create(
        &app,
        &cookie,
        json!({ "title": "Dune", "author": "Frank Herbert", "isbn": "9780131103627", "published_year": null, "total_copies": 1 }),
    )
    .await;

    let (_, _, page) = app.get("/api/books?q=978-0-13-110362-7", None).await;
    assert_eq!(titles(&page), ["Dune"]);
    let (_, _, page) = app.get("/api/books?q=978%200131", None).await;
    assert_eq!(titles(&page), ["Dune"]);
    // Only separators left after stripping: not a match-everything pattern.
    let (_, _, page) = app.get("/api/books?q=-", None).await;
    assert_eq!(page["total"], 0);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn search_longer_than_200_chars_is_rejected(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let (status, _, body) = app
        .get(&format!("/api/books?q={}", "a".repeat(201)), None)
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "validation_error");
    assert_eq!(
        body["error"]["fields"]["q"],
        "must be at most 200 characters"
    );

    let (status, _, _) = app
        .get(&format!("/api/books?q={}", "a".repeat(200)), None)
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn search_treats_wildcards_literally(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    create(&app, &cookie, book("100% Rust")).await;
    create(&app, &cookie, book("snake_case")).await;
    create(&app, &cookie, book("snakeXcase")).await;
    create(&app, &cookie, book("back\\slash")).await;

    let (_, _, page) = app.get("/api/books?q=%25", None).await;
    assert_eq!(titles(&page), ["100% Rust"]);
    let (_, _, page) = app.get("/api/books?q=e_c", None).await;
    assert_eq!(titles(&page), ["snake_case"]);
    let (_, _, page) = app.get("/api/books?q=%5C", None).await;
    assert_eq!(titles(&page), ["back\\slash"]);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn availability_filter(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let free = create(&app, &cookie, book("Free")).await;
    let partial = create(&app, &cookie, book("Partial")).await; // 2 copies
    let full = create(&app, &cookie, json!({ "title": "Full", "author": "A", "isbn": null, "published_year": null, "total_copies": 1 })).await;
    insert_loan(&pool, partial["id"].as_i64().unwrap(), false).await;
    insert_loan(&pool, full["id"].as_i64().unwrap(), false).await;
    // A returned loan does not count.
    insert_loan(&pool, free["id"].as_i64().unwrap(), true).await;

    let (_, _, page) = app.get("/api/books?availability=available", None).await;
    assert_eq!(titles(&page), ["Free", "Partial"]);
    let (_, _, page) = app.get("/api/books?availability=on_loan", None).await;
    assert_eq!(titles(&page), ["Full", "Partial"]);
    let (_, _, page) = app.get("/api/books?availability=all", None).await;
    assert_eq!(page["total"], 3);

    let (_, _, one) = app
        .get(&format!("/api/books/{}", partial["id"]), None)
        .await;
    assert_eq!(one["available_copies"], 1);
    let (_, _, one) = app.get(&format!("/api/books/{}", full["id"]), None).await;
    assert_eq!(one["available_copies"], 0);

    let (status, _, json) = app.get("/api/books?availability=bogus", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["code"], "bad_request");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn pagination_and_ordering(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    for title in ["d", "b", "a", "e", "c"] {
        create(&app, &cookie, book(title)).await;
    }

    let (_, _, page) = app.get("/api/books?per_page=2", None).await;
    assert_eq!(titles(&page), ["a", "b"]);
    assert_eq!(
        (
            page["page"].as_u64(),
            page["per_page"].as_u64(),
            page["total"].as_u64()
        ),
        (Some(1), Some(2), Some(5))
    );
    let (_, _, page) = app.get("/api/books?per_page=2&page=3", None).await;
    assert_eq!(titles(&page), ["e"]);
    let (_, _, page) = app.get("/api/books?per_page=2&page=9", None).await;
    assert!(titles(&page).is_empty());
    assert_eq!(page["total"], 5);

    // Out-of-range values are clamped, not rejected.
    let (status, _, page) = app.get("/api/books?page=0&per_page=500", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (page["page"].as_u64(), page["per_page"].as_u64()),
        (Some(1), Some(100))
    );
    let (_, _, page) = app.get("/api/books", None).await;
    assert_eq!(page["per_page"], 20);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn update_replaces_fields(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, book("Old")).await;
    let uri = format!("/api/books/{}", created["id"]);

    let body = json!({ "title": " New ", "author": "Other", "isbn": null, "published_year": null, "total_copies": 5 });
    let (status, _, json) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["title"], "New");
    assert_eq!(json["total_copies"], 5);
    assert_eq!(json["available_copies"], 5);
    assert_eq!(json["published_year"], Value::Null);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn update_missing_is_404_and_invalid_is_422(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let (status, _, _) = app
        .put_json("/api/books/999", &book("X"), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let created = create(&app, &cookie, book("A")).await;
    let mut bad = book("A");
    bad["total_copies"] = json!(0);
    let (status, _, _) = app
        .put_json(
            &format!("/api/books/{}", created["id"]),
            &bad,
            Some(&cookie),
        )
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn cannot_reduce_copies_below_active_loans(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let mut body = book("A");
    body["total_copies"] = json!(3);
    let created = create(&app, &cookie, body).await;
    let id = created["id"].as_i64().unwrap();
    insert_loan(&pool, id, false).await;
    insert_loan(&pool, id, true).await; // returned: does not count
    let uri = format!("/api/books/{id}");

    // Exactly as many copies as active loans is allowed...
    let mut body = book("A");
    body["total_copies"] = json!(1);
    let (status, _, json) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK, "{json}");

    // ...one fewer than active loans is not.
    insert_loan(&pool, id, false).await;
    let (status, _, json) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "copies_below_active_loans");

    body["total_copies"] = json!(2);
    let (status, _, json) = app.put_json(&uri, &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["available_copies"], 0);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn delete_happy_path_and_404(pool: SqlitePool) {
    let app = TestApp::new(pool);
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, book("A")).await;
    let uri = format!("/api/books/{}", created["id"]);

    let (status, _, body) = app.delete(&uri, Some(&cookie)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_eq!(body, Value::Null);
    let (status, _, _) = app.get(&uri, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _, _) = app.delete(&uri, Some(&cookie)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn delete_with_active_loan_is_409(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, book("A")).await;
    let id = created["id"].as_i64().unwrap();
    insert_loan(&pool, id, false).await;

    let (status, _, json) = app.delete(&format!("/api/books/{id}"), Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "book_has_active_loans");
    let (status, _, _) = app.get(&format!("/api/books/{id}"), None).await;
    assert_eq!(status, StatusCode::OK);
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn delete_with_only_returned_loans_removes_them(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let created = create(&app, &cookie, book("A")).await;
    let id = created["id"].as_i64().unwrap();
    insert_loan(&pool, id, true).await;

    let (status, _, _) = app.delete(&format!("/api/books/{id}"), Some(&cookie)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let loans: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM loans")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(loans, 0);
}

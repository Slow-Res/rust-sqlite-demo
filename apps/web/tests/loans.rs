//! Integration tests for `/api/loans`, including the "Borrow a book" walkthrough.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use common::{TestApp, insert_loan, start_time};
use serde_json::{Value, json};

/// Create a book with `copies` copies through the API; returns its id.
async fn add_book(app: &TestApp, cookie: &str, title: &str, copies: i64) -> i64 {
    let body = json!({ "title": title, "author": "Author", "total_copies": copies });
    let (status, _, json) = app.post_json("/api/books", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json["id"].as_i64().unwrap()
}

async fn add_member(app: &TestApp, cookie: &str, name: &str) -> i64 {
    let email = format!("{}@example.com", name.to_lowercase());
    let body = json!({ "name": name, "email": email });
    let (status, _, json) = app.post_json("/api/members", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json["id"].as_i64().unwrap()
}

/// Borrow through the API and return the loan JSON.
async fn borrow(app: &TestApp, cookie: &str, book_id: i64, member_id: i64) -> Value {
    let body = json!({ "book_id": book_id, "member_id": member_id });
    let (status, _, json) = app.post_json("/api/loans", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json
}

async fn give_back(app: &TestApp, cookie: &str, loan_id: i64) -> Value {
    let uri = format!("/api/loans/{loan_id}/return");
    let (status, _, json) = app.post_json(&uri, &json!({}), Some(cookie)).await;
    assert_eq!(status, StatusCode::OK, "{json}");
    json
}

/// Log in again as an existing user. Sessions last 7 days, so tests that move
/// the clock further than that need a fresh cookie.
async fn relogin(app: &TestApp, username: &str, password: &str) -> String {
    let body = json!({ "username": username, "password": password });
    let (status, headers, _) = app.post_json("/api/auth/login", &body, None).await;
    assert_eq!(status, StatusCode::OK);
    let set_cookie = headers.get("set-cookie").unwrap().to_str().unwrap();
    set_cookie.split(';').next().unwrap().to_owned()
}

fn ids(page: &Value) -> Vec<i64> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["id"].as_i64().unwrap())
        .collect()
}

fn time(value: &Value) -> DateTime<Utc> {
    value.as_str().unwrap().parse().unwrap()
}

/// Fire one borrow per `(book, member)` pair at once and count the outcomes:
/// `(created, refused with `code`)`. Any other status fails the test.
async fn race_borrows(
    app: &TestApp,
    cookie: &str,
    pairs: Vec<(i64, i64)>,
    code: &str,
) -> (usize, usize) {
    let handles: Vec<_> = pairs
        .into_iter()
        .map(|(book, member)| {
            let app = app.clone();
            let cookie = cookie.to_owned();
            tokio::spawn(async move {
                let body = json!({ "book_id": book, "member_id": member });
                app.post_json("/api/loans", &body, Some(&cookie)).await
            })
        })
        .collect();
    let (mut created, mut refused) = (0, 0);
    for handle in handles {
        let (status, _, json) = handle.await.unwrap();
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {
                assert_eq!(json["error"]["code"], code, "{json}");
                refused += 1;
            }
            other => panic!("unexpected status {other}: {json}"),
        }
    }
    (created, refused)
}

db_tests! {
async fn every_endpoint_requires_auth(db: Database) {
    let app = TestApp::new(db);
    let body = json!({ "book_id": 1, "member_id": 1 });
    let (status, _, _) = app.get("/api/loans", None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = app.post_json("/api/loans", &body, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = app.post_json("/api/loans/1/return", &body, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

async fn borrowing_creates_a_loan_due_in_fourteen_days(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 2).await;
    let member = add_member(&app, &cookie, "Grace").await;

    let body = json!({ "book_id": book, "member_id": member });
    let (status, _, loan) = app.post_json("/api/loans", &body, Some(&cookie)).await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(loan["book_id"], book);
    assert_eq!(loan["book_title"], "Dune");
    assert_eq!(loan["member_id"], member);
    assert_eq!(loan["member_name"], "Grace");
    assert_eq!(time(&loan["borrowed_at"]), start_time());
    assert_eq!(time(&loan["due_at"]), start_time() + Duration::days(14));
    assert_eq!(loan["returned_at"], Value::Null);
    assert_eq!(loan["is_overdue"], false);
}

async fn borrowing_and_returning_move_the_derived_counts(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 3).await;
    let member = add_member(&app, &cookie, "Grace").await;
    let book_uri = format!("/api/books/{book}");
    let member_uri = format!("/api/members/{member}");

    let loan = borrow(&app, &cookie, book, member).await;
    let (_, _, b) = app.get(&book_uri, None).await;
    let (_, _, m) = app.get(&member_uri, Some(&cookie)).await;
    assert_eq!(b["available_copies"], 2);
    assert_eq!(m["active_loans"], 1);

    give_back(&app, &cookie, loan["id"].as_i64().unwrap()).await;
    let (_, _, b) = app.get(&book_uri, None).await;
    let (_, _, m) = app.get(&member_uri, Some(&cookie)).await;
    assert_eq!(b["available_copies"], 3);
    assert_eq!(m["active_loans"], 0);
}

async fn non_positive_ids_are_422(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let body = json!({ "book_id": 0, "member_id": -1 });
    let (status, _, json) = app.post_json("/api/loans", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json["error"]["fields"]["book_id"], "must be a positive id");
    assert_eq!(
        json["error"]["fields"]["member_id"],
        "must be a positive id"
    );
}

async fn unknown_book_or_member_is_404(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 1).await;
    let member = add_member(&app, &cookie, "Grace").await;

    let post = |book_id: i64, member_id: i64| {
        let body = json!({ "book_id": book_id, "member_id": member_id });
        let (app, cookie) = (&app, &cookie);
        async move { app.post_json("/api/loans", &body, Some(cookie)).await }
    };
    let (status, _, json) = post(999, member).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["message"], "book not found");
    let (status, _, json) = post(book, 999).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["message"], "member not found");
    // Both unknown: the book is checked first.
    let (_, _, json) = post(999, 999).await;
    assert_eq!(json["error"]["message"], "book not found");
}

async fn no_free_copy_is_409(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 1).await;
    let a = add_member(&app, &cookie, "Ann").await;
    let b = add_member(&app, &cookie, "Bob").await;
    borrow(&app, &cookie, book, a).await;

    let body = json!({ "book_id": book, "member_id": b });
    let (status, _, json) = app.post_json("/api/loans", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "no_copies_available");
    assert_eq!(
        json["error"]["message"],
        "no copies of this book are available"
    );
}

async fn sixth_active_loan_is_409(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let member = add_member(&app, &cookie, "Grace").await;
    let mut books = Vec::new();
    for n in 1..=6 {
        books.push(add_book(&app, &cookie, &format!("Book {n}"), 1).await);
    }
    let mut loan_ids = Vec::new();
    for &book in &books[..5] {
        let loan = borrow(&app, &cookie, book, member).await;
        loan_ids.push(loan["id"].as_i64().unwrap());
    }

    let body = json!({ "book_id": books[5], "member_id": member });
    let (status, _, json) = app.post_json("/api/loans", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "loan_limit_reached");
    assert_eq!(
        json["error"]["message"],
        "member already has the maximum of 5 active loans"
    );

    // Returning one frees a slot.
    give_back(&app, &cookie, loan_ids[0]).await;
    borrow(&app, &cookie, books[5], member).await;
}

async fn returning_sets_returned_at_once(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 1).await;
    let member = add_member(&app, &cookie, "Grace").await;
    let loan = borrow(&app, &cookie, book, member).await;
    let id = loan["id"].as_i64().unwrap();

    app.clock.advance(Duration::days(3));
    let returned = give_back(&app, &cookie, id).await;
    assert_eq!(
        time(&returned["returned_at"]),
        start_time() + Duration::days(3)
    );
    assert_eq!(returned["book_title"], "Dune");

    let uri = format!("/api/loans/{id}/return");
    let (status, _, json) = app.post_json(&uri, &json!({}), Some(&cookie)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json["error"]["code"], "loan_already_returned");

    let (status, _, json) = app
        .post_json("/api/loans/999/return", &json!({}), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json["error"]["message"], "loan not found");
}

async fn list_filters_by_status(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 5).await;
    let member = add_member(&app, &cookie, "Grace").await;
    let old = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();
    let done = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();
    give_back(&app, &cookie, done).await;

    // Exactly at the due date the loan is not overdue yet.
    app.clock.advance(Duration::days(14));
    let cookie = relogin(&app, "ada", "password123").await;
    let (_, _, overdue) = app.get("/api/loans?status=overdue", Some(&cookie)).await;
    assert_eq!(ids(&overdue), Vec::<i64>::new());

    app.clock.advance(Duration::days(1));
    let (_, _, overdue) = app.get("/api/loans?status=overdue", Some(&cookie)).await;
    assert_eq!(ids(&overdue), [old]);
    assert_eq!(overdue["items"][0]["is_overdue"], true);

    // A fresh loan made now is active but not overdue; default status is `active`.
    let fresh = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();
    let (_, _, active) = app.get("/api/loans", Some(&cookie)).await;
    assert_eq!(ids(&active), [old, fresh]);
    let (_, _, explicit) = app.get("/api/loans?status=active", Some(&cookie)).await;
    assert_eq!(ids(&explicit), [old, fresh]);

    let (_, _, returned) = app.get("/api/loans?status=returned", Some(&cookie)).await;
    assert_eq!(ids(&returned), [done]);
    assert_eq!(returned["items"][0]["is_overdue"], false);
    let (_, _, all) = app.get("/api/loans?status=all", Some(&cookie)).await;
    assert_eq!(ids(&all), [old, done, fresh]);
    assert_eq!(all["total"], 3);
}

async fn list_filters_by_member_and_book(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let dune = add_book(&app, &cookie, "Dune", 3).await;
    let emma = add_book(&app, &cookie, "Emma", 3).await;
    let ann = add_member(&app, &cookie, "Ann").await;
    let bob = add_member(&app, &cookie, "Bob").await;
    let l1 = borrow(&app, &cookie, dune, ann).await["id"]
        .as_i64()
        .unwrap();
    let l2 = borrow(&app, &cookie, emma, ann).await["id"]
        .as_i64()
        .unwrap();
    let l3 = borrow(&app, &cookie, dune, bob).await["id"]
        .as_i64()
        .unwrap();

    let (_, _, page) = app
        .get(&format!("/api/loans?member_id={ann}"), Some(&cookie))
        .await;
    assert_eq!(ids(&page), [l1, l2]);
    let (_, _, page) = app
        .get(&format!("/api/loans?book_id={dune}"), Some(&cookie))
        .await;
    assert_eq!(ids(&page), [l1, l3]);
    let (_, _, page) = app
        .get(
            &format!("/api/loans?book_id={dune}&member_id={bob}"),
            Some(&cookie),
        )
        .await;
    assert_eq!(ids(&page), [l3]);
}

async fn list_ordering_follows_the_status(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune", 5).await;
    let member = add_member(&app, &cookie, "Grace").await;
    let first = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();
    // Same due date as `first`: id breaks the tie.
    let second = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();
    app.clock.advance(Duration::hours(1));
    let third = borrow(&app, &cookie, book, member).await["id"]
        .as_i64()
        .unwrap();

    let (_, _, active) = app.get("/api/loans", Some(&cookie)).await;
    assert_eq!(ids(&active), [first, second, third]);

    // Returned loans: most recently returned first, id DESC on ties.
    give_back(&app, &cookie, third).await;
    app.clock.advance(Duration::hours(1));
    give_back(&app, &cookie, first).await;
    give_back(&app, &cookie, second).await;
    let (_, _, returned) = app.get("/api/loans?status=returned", Some(&cookie)).await;
    assert_eq!(ids(&returned), [second, first, third]);
    // `all` is ordered like the active lists.
    let (_, _, all) = app.get("/api/loans?status=all", Some(&cookie)).await;
    assert_eq!(ids(&all), [first, second, third]);
}

async fn list_is_paginated(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let member = add_member(&app, &cookie, "Grace").await;
    for _ in 0..3 {
        insert_loan(&app, member, false).await;
    }

    let (_, _, page) = app.get("/api/loans?page=2&per_page=2", Some(&cookie)).await;
    assert_eq!(page["total"], 3);
    assert_eq!(page["page"], 2);
    assert_eq!(page["per_page"], 2);
    assert_eq!(ids(&page).len(), 1);
}

async fn bad_query_params_are_400(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let (status, _, json) = app.get("/api/loans?status=lost", Some(&cookie)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["code"], "bad_request");
}

async fn concurrent_borrows_never_exceed_total_copies(db: Database) {
    const BORROWERS: usize = 8;
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Scarce", 2).await;
    let mut members = Vec::new();
    for n in 0..BORROWERS {
        members.push(add_member(&app, &cookie, &format!("Member{n}")).await);
    }

    // All requests race through clones of the same router and database pool.
    let handles: Vec<_> = members
        .into_iter()
        .map(|member| {
            let app = app.clone();
            let cookie = cookie.clone();
            tokio::spawn(async move {
                let body = json!({ "book_id": book, "member_id": member });
                app.post_json("/api/loans", &body, Some(&cookie)).await
            })
        })
        .collect();

    let mut created = 0;
    let mut refused = 0;
    for handle in handles {
        let (status, _, json) = handle.await.unwrap();
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::CONFLICT => {
                assert_eq!(json["error"]["code"], "no_copies_available", "{json}");
                refused += 1;
            }
            other => panic!("unexpected status {other}: {json}"),
        }
    }
    assert_eq!((created, refused), (2, BORROWERS - 2));

    let active = app
        .scalar(
            "SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL",
            &[book.into()],
        )
        .await;
    assert_eq!(active, 2);
}

async fn ten_members_race_for_the_last_copy(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Last copy", 1).await;
    let mut pairs = Vec::new();
    for n in 0..10 {
        pairs.push((book, add_member(&app, &cookie, &format!("Member{n}")).await));
    }

    let outcome = race_borrows(&app, &cookie, pairs, "no_copies_available").await;
    assert_eq!(outcome, (1, 9));

    let active = app
        .scalar(
            "SELECT COUNT(*) FROM loans WHERE book_id = $1 AND returned_at IS NULL",
            &[book.into()],
        )
        .await;
    assert_eq!(active, 1);
}

async fn one_member_racing_never_exceeds_the_loan_limit(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let member = add_member(&app, &cookie, "Grace").await;
    let mut pairs = Vec::new();
    for n in 0..12 {
        let book = add_book(&app, &cookie, &format!("Book {n}"), 1).await;
        pairs.push((book, member));
    }

    let outcome = race_borrows(&app, &cookie, pairs, "loan_limit_reached").await;
    assert_eq!(outcome, (5, 7));

    let active = app
        .scalar(
            "SELECT COUNT(*) FROM loans WHERE member_id = $1 AND returned_at IS NULL",
            &[member.into()],
        )
        .await;
    assert_eq!(active, 5);
}
}

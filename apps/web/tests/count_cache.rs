//! The list-total cache seen through HTTP: only the total of an unfiltered list
//! may lag, never the rows, and filtered totals are always exact.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};

/// Long enough that no test run outlives it.
const TTL_MS: u64 = 600_000;

async fn add_book(app: &TestApp, cookie: &str, title: &str) {
    let body = json!({ "title": title, "author": "Some Author", "isbn": null, "published_year": 2000, "total_copies": 2 });
    let (status, _, json) = app.post_json("/api/books", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
}

async fn add_member(app: &TestApp, cookie: &str, n: u32) {
    let body = json!({ "name": format!("Member {n}"), "email": format!("m{n}@example.com") });
    let (status, _, json) = app.post_json("/api/members", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
}

async fn page(app: &TestApp, uri: &str, cookie: &str) -> Value {
    let (status, _, page) = app.get(uri, Some(cookie)).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    page
}

fn len(page: &Value) -> usize {
    page["items"].as_array().unwrap().len()
}

db_tests! {
async fn the_unfiltered_book_total_may_lag_but_the_rows_and_searches_do_not(db: Database) {
    let app = TestApp::with_count_ttl(db, TTL_MS);
    let cookie = app.login("ada", "password123").await;
    add_book(&app, &cookie, "Dune").await;
    assert_eq!(page(&app, "/api/books", &cookie).await["total"], 1);

    add_book(&app, &cookie, "Dune Messiah").await;
    let all = page(&app, "/api/books", &cookie).await;
    assert_eq!(len(&all), 2, "the rows are read every time");
    assert_eq!(all["total"], 1, "the total is the cached one");

    // A search is counted for real, and so is a filter on availability.
    assert_eq!(page(&app, "/api/books?q=Dune", &cookie).await["total"], 2);
    assert_eq!(page(&app, "/api/books?availability=available", &cookie).await["total"], 2);
}

async fn the_unfiltered_member_total_may_lag_but_a_search_total_does_not(db: Database) {
    let app = TestApp::with_count_ttl(db, TTL_MS);
    let cookie = app.login("ada", "password123").await;
    add_member(&app, &cookie, 1).await;
    assert_eq!(page(&app, "/api/members", &cookie).await["total"], 1);

    add_member(&app, &cookie, 2).await;
    let all = page(&app, "/api/members", &cookie).await;
    assert_eq!(len(&all), 2);
    assert_eq!(all["total"], 1);
    assert_eq!(page(&app, "/api/members?q=Member", &cookie).await["total"], 2);
}

async fn each_loan_status_has_its_own_total_and_overdue_is_never_cached(db: Database) {
    let app = TestApp::with_count_ttl(db, TTL_MS);
    let cookie = app.login("ada", "password123").await;
    add_member(&app, &cookie, 1).await;
    let member_id = app.scalar("SELECT id FROM members", &[]).await;
    common::insert_loan(&app, member_id, false).await;
    common::insert_loan(&app, member_id, true).await;
    let (_, _, active) = app.get("/api/loans?status=active", Some(&cookie)).await;
    let (_, _, returned) = app.get("/api/loans?status=returned", Some(&cookie)).await;
    assert_eq!((active["total"].clone(), returned["total"].clone()), (json!(1), json!(1)));

    common::insert_loan(&app, member_id, false).await;
    common::insert_loan(&app, member_id, true).await;
    let (_, _, active) = app.get("/api/loans?status=active", Some(&cookie)).await;
    let (_, _, returned) = app.get("/api/loans?status=returned", Some(&cookie)).await;
    let (_, _, all) = app.get("/api/loans?status=all", Some(&cookie)).await;
    assert_eq!(len(&active), 2);
    assert_eq!(active["total"], 1, "cached");
    assert_eq!(returned["total"], 1, "cached");
    assert_eq!(all["total"], 4, "first count of this status, so exact");

    // Overdue is counted every time: both unreturned loans are past due.
    let (_, _, overdue) = app.get("/api/loans?status=overdue", Some(&cookie)).await;
    assert_eq!(overdue["total"], 2);
    common::insert_loan(&app, member_id, false).await;
    let (_, _, overdue) = app.get("/api/loans?status=overdue", Some(&cookie)).await;
    assert_eq!(overdue["total"], 3);
    // A filter on the member bypasses the cache too.
    let uri = format!("/api/loans?status=active&member_id={member_id}");
    let (_, _, mine) = app.get(&uri, Some(&cookie)).await;
    assert_eq!(mine["total"], 3);
}
}

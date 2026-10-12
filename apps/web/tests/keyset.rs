//! Keyset ("next page") pagination: the `after` cursor and `next_cursor` of the
//! three list endpoints, on both backends.
// Helpers (unlike `#[test]` fns) are not covered by clippy.toml's unwrap allowance.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::TestApp;
use serde_json::{Value, json};

async fn add_book(app: &TestApp, cookie: &str, title: &str) -> i64 {
    let body = json!({ "title": title, "author": "Author", "total_copies": 5 });
    let (status, _, json) = app.post_json("/api/books", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json["id"].as_i64().unwrap()
}

async fn add_member(app: &TestApp, cookie: &str, name: &str, email: &str) -> i64 {
    let body = json!({ "name": name, "email": email });
    let (status, _, json) = app.post_json("/api/members", &body, Some(cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{json}");
    json["id"].as_i64().unwrap()
}

/// A loan straight into the database, so due and return times can tie.
async fn add_loan(app: &TestApp, book: i64, member: i64, due: i64, returned: Option<i64>) -> i64 {
    app.scalar(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES ($1, $2, 0, $3, $4) RETURNING id",
        &[book.into(), member.into(), due.into(), returned.into()],
    )
    .await
}

fn ids(items: &[Value]) -> Vec<i64> {
    items.iter().map(|i| i["id"].as_i64().unwrap()).collect()
}

/// Follow `next_cursor` from the first page to the end. Every page but the last
/// must be full; the last must have a null `next_cursor`. Returns all items.
async fn walk(app: &TestApp, cookie: &str, base: &str, per_page: usize) -> Vec<Value> {
    let sep = if base.contains('?') { '&' } else { '?' };
    let mut items = Vec::new();
    let mut after: Option<String> = None;
    for _ in 0..50 {
        let mut uri = format!("{base}{sep}per_page={per_page}");
        if let Some(cursor) = &after {
            uri.push_str(&format!("&after={cursor}"));
        }
        let (status, _, page) = app.get(&uri, Some(cookie)).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {page}");
        let got = page["items"].as_array().unwrap();
        items.extend(got.iter().cloned());
        match page["next_cursor"].as_str() {
            Some(cursor) => {
                assert_eq!(got.len(), per_page, "a page with a next page is full");
                after = Some(cursor.to_owned());
            }
            None => return items,
        }
    }
    panic!("{base}: the walk did not end");
}

/// A cursor holding `payload`, shaped like a real one (base64 of JSON).
fn cursor_of(payload: &str) -> String {
    URL_SAFE_NO_PAD.encode(payload)
}

db_tests! {
async fn books_walk_visits_every_book_once_in_order(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    // Ties on title: the id decides. Titles also differ in case and punctuation.
    let mut expected = Vec::new();
    for title in ["Same", "Alpha", "Same", "Zed", "Same", "Same Tale", "Same", "Same Tale"] {
        expected.push((title, add_book(&app, &cookie, title).await));
    }
    expected.sort();
    let expected: Vec<i64> = expected.into_iter().map(|(_, id)| id).collect();

    // 8 books: per_page 3 leaves a short last page, 4 and 2 end exactly on a boundary.
    for per_page in [3, 4, 2, 1, 20] {
        let items = walk(&app, &cookie, "/api/books", per_page).await;
        assert_eq!(ids(&items), expected, "per_page {per_page}");
    }
}

async fn books_walk_applies_the_filter(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let mut expected = Vec::new();
    for title in ["Same Tale", "Other", "Same Tale", "A Same Tale", "Nope", "Same Tale"] {
        let id = add_book(&app, &cookie, title).await;
        if title.contains("Same") {
            expected.push((title, id));
        }
    }
    expected.sort();
    let expected: Vec<i64> = expected.into_iter().map(|(_, id)| id).collect();

    let items = walk(&app, &cookie, "/api/books?q=Same", 3).await;
    assert_eq!(ids(&items), expected);
    let items = walk(&app, &cookie, "/api/books?q=Same&availability=available", 2).await;
    assert_eq!(ids(&items), expected);

    // The total counts every match, also on a page reached by a cursor and on
    // one past the end (Postgres takes it from a window, or counts when empty).
    let (_, _, first) = app.get("/api/books?q=Same&per_page=2", Some(&cookie)).await;
    assert_eq!(first["total"], expected.len());
    let cursor = first["next_cursor"].as_str().unwrap();
    let uri = format!("/api/books?q=Same&per_page=2&after={cursor}");
    let (_, _, second) = app.get(&uri, Some(&cookie)).await;
    assert_eq!(second["items"].as_array().unwrap().len(), 2);
    assert_eq!(second["total"], expected.len());
    let (_, _, past) = app.get("/api/books?q=Same&per_page=2&page=9", Some(&cookie)).await;
    assert!(past["items"].as_array().unwrap().is_empty());
    assert_eq!(past["total"], expected.len());
}

async fn members_walk_visits_every_member_once_in_order(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let mut expected = Vec::new();
    for (n, name) in ["Sam", "Al", "Sam", "Zoe", "Sam", "Sam", "Bo"].into_iter().enumerate() {
        let id = add_member(&app, &cookie, name, &format!("m{n}@x.io")).await;
        expected.push((name, id));
    }
    expected.sort();
    let expected: Vec<i64> = expected.into_iter().map(|(_, id)| id).collect();

    for per_page in [3, 7, 2, 1] {
        let items = walk(&app, &cookie, "/api/members", per_page).await;
        assert_eq!(ids(&items), expected, "per_page {per_page}");
    }
    // With a filter: only the four Sams, in id order.
    let sams: Vec<i64> = expected
        .iter()
        .copied()
        .filter(|id| *id != expected[0] && *id != expected[1] && *id != expected[6])
        .collect();
    let items = walk(&app, &cookie, "/api/members?q=Sam", 3).await;
    assert_eq!(ids(&items), sams);
}

async fn loans_walk_follows_each_status_order(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let book = add_book(&app, &cookie, "Dune").await;
    let member = add_member(&app, &cookie, "Grace", "g@x.io").await;
    // (due_at, returned_at): ties on both keys. The clock is far past every due
    // date, so every unreturned loan is also overdue.
    let rows = [
        (100, None),
        (100, None),
        (50, None),
        (200, None),
        (100, Some(40)),
        (90, Some(40)),
        (100, Some(30)),
        (60, Some(40)),
        (70, Some(50)),
    ];
    let mut loans = Vec::new();
    for (due, returned) in rows {
        loans.push((due, returned, add_loan(&app, book, member, due, returned).await));
    }

    let by_due = |keep: fn(&Option<i64>) -> bool| -> Vec<i64> {
        let mut rows: Vec<_> = loans.iter().filter(|l| keep(&l.1)).map(|l| (l.0, l.2)).collect();
        rows.sort();
        rows.into_iter().map(|(_, id)| id).collect()
    };
    let mut returned: Vec<_> = loans.iter().filter_map(|l| l.1.map(|r| (r, l.2))).collect();
    returned.sort();
    returned.reverse(); // returned_at DESC, id DESC
    let returned: Vec<i64> = returned.into_iter().map(|(_, id)| id).collect();

    let cases = [
        ("active", by_due(|r| r.is_none())),
        ("overdue", by_due(|r| r.is_none())),
        ("all", by_due(|_| true)),
        ("returned", returned),
    ];
    for (status, expected) in cases {
        for per_page in [2, 3, 1] {
            let base = format!("/api/loans?status={status}");
            let items = walk(&app, &cookie, &base, per_page).await;
            assert_eq!(ids(&items), expected, "{status}, per_page {per_page}");
        }
    }
}

async fn last_page_has_no_next_cursor(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    for title in ["A", "B", "C"] {
        add_book(&app, &cookie, title).await;
    }
    // Exactly per_page items left: still null.
    let (_, _, page) = app.get("/api/books?per_page=3", Some(&cookie)).await;
    assert_eq!(page["items"].as_array().unwrap().len(), 3);
    assert_eq!(page["next_cursor"], Value::Null);
    let (_, _, page) = app.get("/api/books?per_page=2", Some(&cookie)).await;
    assert!(page["next_cursor"].is_string());
    let cursor = page["next_cursor"].as_str().unwrap();
    let (_, _, last) = app.get(&format!("/api/books?per_page=2&after={cursor}"), Some(&cookie)).await;
    assert_eq!(last["items"].as_array().unwrap().len(), 1);
    assert_eq!(last["next_cursor"], Value::Null);
    // Past the end: empty, null.
    let last_id = last["items"][0]["id"].as_i64().unwrap();
    let cursor = cursor_of(&format!(r#"["C",{last_id}]"#));
    let (_, _, empty) = app
        .get(&format!("/api/books?after={cursor}"), Some(&cookie))
        .await;
    assert_eq!(empty["items"], json!([]));
    assert_eq!(empty["next_cursor"], Value::Null);
    assert_eq!(empty["total"], 3);
}

async fn page_mode_still_works_and_sets_next_cursor(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let mut expected = Vec::new();
    for title in ["A", "B", "C", "D", "E"] {
        expected.push(add_book(&app, &cookie, title).await);
    }

    let (_, _, page2) = app.get("/api/books?page=2&per_page=2", Some(&cookie)).await;
    assert_eq!(ids(page2["items"].as_array().unwrap()), expected[2..4]);
    assert_eq!(page2["page"], 2);
    assert_eq!(page2["total"], 5);
    let (_, _, page3) = app.get("/api/books?page=3&per_page=2", Some(&cookie)).await;
    assert_eq!(page3["next_cursor"], Value::Null);

    // Page mode's cursor continues where it stopped; `page` is ignored then.
    let cursor = page2["next_cursor"].as_str().unwrap();
    let uri = format!("/api/books?page=9&per_page=2&after={cursor}");
    let (status, _, next) = app.get(&uri, Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ids(next["items"].as_array().unwrap()), expected[4..]);

    // The same for the other two lists.
    let (_, _, members) = app.get("/api/members?per_page=1", Some(&cookie)).await;
    assert_eq!(members["next_cursor"], Value::Null);
    let (_, _, loans) = app.get("/api/loans?per_page=1", Some(&cookie)).await;
    assert_eq!(loans["next_cursor"], Value::Null);
}

async fn malformed_cursors_are_400(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let everywhere = [
        "!!!".to_owned(),
        "a".to_owned(),
        cursor_of("not json"),
        cursor_of("{}"),
        cursor_of(r#"["only-one"]"#),
        cursor_of(r#"["T",1,2]"#),
        cursor_of(r#"["T","x"]"#),
    ];
    // The key type is per list: text for books and members, a timestamp for loans.
    let number_key = cursor_of("[1,2]");
    let text_key = cursor_of(r#"["T",1]"#);
    let cases = [
        ("/api/books", &number_key),
        ("/api/members", &number_key),
        ("/api/loans", &text_key),
    ];
    for (uri, wrong_type) in cases {
        for cursor in everywhere.iter().chain([wrong_type]) {
            let (status, _, json) = app.get(&format!("{uri}?after={cursor}"), Some(&cookie)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}?after={cursor}: {json}");
            assert_eq!(json["error"]["code"], "bad_request");
        }
    }
}

async fn cursor_text_is_bound_not_executed(db: Database) {
    let app = TestApp::new(db);
    let cookie = app.login("ada", "password123").await;
    let id = add_book(&app, &cookie, "Dune").await;
    let evil = cursor_of(r#"["x') OR 1=1 --", 0]"#);
    let (status, _, page) = app.get(&format!("/api/books?after={evil}"), Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    // "Dune" < "x...", so nothing comes after it, and the books table is intact.
    assert_eq!(page["items"], json!([]));
    assert_eq!(app.scalar("SELECT id FROM books", &[]).await, id);
}
}

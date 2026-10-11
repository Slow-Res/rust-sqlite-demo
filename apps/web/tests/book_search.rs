//! Book search (`GET /api/books?q=`): the FTS5 trigram path, the `LIKE`
//! fallback for short queries, and the index staying in sync with `books`.
//!
//! Raw SQL here uses `$1` placeholders, which SQLite also accepts, so the
//! statements stay portable to the Postgres backend.
#![allow(clippy::unwrap_used)]

mod common;

use axum::http::StatusCode;
use common::TestApp;
use serde_json::{Value, json};
use sqlx::SqlitePool;

/// Books by name, so a test can say which ones it expects.
struct Fixture {
    hobbit: i64,
    dune: i64,
    hobbit_cookbook: i64,
    fellowship: i64,
    pure_joy: i64,
    hello: i64,
    cafe: i64,
}

async fn insert_book(pool: &SqlitePool, title: &str, author: &str, isbn: Option<&str>) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO books (title, author, isbn, total_copies, created_at, updated_at) \
         VALUES ($1, $2, $3, 1, 0, 0) RETURNING id",
    )
    .bind(title)
    .bind(author)
    .bind(isbn)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn fixture(pool: &SqlitePool) -> Fixture {
    Fixture {
        hobbit: insert_book(pool, "The Hobbit", "J.R.R. Tolkien", Some("9780261102217")).await,
        dune: insert_book(pool, "Dune", "Frank Herbert", Some("9780441172719")).await,
        hobbit_cookbook: insert_book(
            pool,
            "Hobbit Hole Cookbook",
            "Sam Gamgee",
            Some("9781234567897"),
        )
        .await,
        fellowship: insert_book(pool, "The Fellowship of the Ring", "J.R.R. Tolkien", None).await,
        pure_joy: insert_book(pool, "100% Pure_Joy", "Ann Author", None).await,
        hello: insert_book(pool, "Say \"Hello\" World", "Bob Writer", None).await,
        cafe: insert_book(pool, "Café Société", "Zoë Müller", None).await,
    }
}

/// Percent-encode everything but unreserved characters.
fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                char::from(b).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Ids of the matching books (sorted) and the reported total.
async fn search(app: &TestApp, q: &str, extra: &str) -> (Vec<i64>, u64) {
    let uri = format!("/api/books?per_page=100&q={}{extra}", encode(q));
    let (status, _, page) = app.get(&uri, None).await;
    assert_eq!(status, StatusCode::OK, "{q}: {page}");
    (ids(&page), page["total"].as_u64().unwrap())
}

fn ids(page: &Value) -> Vec<i64> {
    let mut ids: Vec<i64> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["id"].as_i64().unwrap())
        .collect();
    ids.sort_unstable();
    ids
}

fn sorted(mut ids: Vec<i64>) -> Vec<i64> {
    ids.sort_unstable();
    ids
}

/// Assert that `q` finds exactly `expected`, and that `total` agrees.
async fn assert_finds(app: &TestApp, q: &str, expected: Vec<i64>) {
    let expected = sorted(expected);
    let (found, total) = search(app, q, "").await;
    assert_eq!(found, expected, "q = {q:?}");
    assert_eq!(total, expected.len() as u64, "total for q = {q:?}");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn title_substring_ignores_case(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    assert_finds(&app, "hobbit", vec![f.hobbit, f.hobbit_cookbook]).await;
    assert_finds(&app, "HoBbIt", vec![f.hobbit, f.hobbit_cookbook]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn author_substring(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    assert_finds(&app, "tolkien", vec![f.hobbit, f.fellowship]).await;
    assert_finds(&app, "Gamgee", vec![f.hobbit_cookbook]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn isbn_substring_and_typed_with_hyphens_and_spaces(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    assert_finds(&app, "0261102", vec![f.hobbit]).await;
    // Stored without separators; the typed form has them.
    assert_finds(&app, "978-0-261 10221-7", vec![f.hobbit]).await;
    assert_finds(&app, "0441 172-719", vec![f.dune]).await;
    assert_finds(&app, "000-000", vec![]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn wildcards_and_quotes_match_literally(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    assert_finds(&app, "100%", vec![f.pure_joy]).await;
    assert_finds(&app, "e_Joy", vec![f.pure_joy]).await;
    // `LIKE` wildcards would match these; literal text does not.
    assert_finds(&app, "%%%", vec![]).await;
    assert_finds(&app, "Pure%Joy", vec![]).await;
    assert_finds(&app, "Pur_ Joy", vec![]).await;
    assert_finds(&app, "\"Hello\"", vec![f.hello]).await;
    assert_finds(&app, "y \"H", vec![f.hello]).await;
    // FTS5 syntax is just text.
    assert_finds(&app, "title:Dune", vec![]).await;
    assert_finds(&app, "Dune OR Hobbit", vec![]).await;
    assert_finds(&app, "Dun*", vec![]).await;
    assert_finds(&app, "(Dune)", vec![]).await;
    // A lone quote is a 1-character query (the scan); three of them go to the index.
    assert_finds(&app, "\"", vec![f.hello]).await;
    assert_finds(&app, "\"\"\"", vec![]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn non_ascii_title_in_the_stored_case(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    // Same case as stored. How other-case non-ASCII text behaves differs by backend.
    assert_finds(&app, "Café", vec![f.cafe]).await;
    assert_finds(&app, "Société", vec![f.cafe]).await;
    assert_finds(&app, "Müller", vec![f.cafe]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn short_queries_fall_back_to_like(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    // 2 characters: the title/author part.
    assert_finds(&app, "zo", vec![f.cafe]).await;
    assert_finds(&app, "ZO", vec![f.cafe]).await;
    // "ë" is 2 bytes but 1 character, so "oë" is short too.
    assert_finds(&app, "oë", vec![f.cafe]).await;
    // 1 character, matched in the ISBNs.
    assert_finds(&app, "9", vec![f.hobbit, f.dune, f.hobbit_cookbook]).await;
    // Long enough, but with its separators removed only "97" is left: still the scan.
    assert_finds(&app, "9 7", vec![f.hobbit, f.dune, f.hobbit_cookbook]).await;
    // `%` is a single character here and must still be literal.
    assert_finds(&app, "%", vec![f.pure_joy]).await;
    assert_finds(&app, "_", vec![f.pure_joy]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn multi_word_query_is_one_phrase(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool);
    assert_finds(&app, "fellowship of the", vec![f.fellowship]).await;
    assert_finds(&app, "the hobbit", vec![f.hobbit]).await;
    // Both words occur in "The Hobbit" / "J.R.R. Tolkien", but not as one string.
    assert_finds(&app, "hobbit tolkien", vec![]).await;
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn combined_with_availability(pool: SqlitePool) {
    let f = fixture(&pool).await;
    // The only copy of "The Hobbit" is out.
    let member_id: i64 = sqlx::query_scalar(
        "INSERT INTO members (name, email, created_at, updated_at) \
         VALUES ('M', 'm@example.com', 0, 0) RETURNING id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1, $2, 0, 100)",
    )
    .bind(f.hobbit)
    .bind(member_id)
    .execute(&pool)
    .await
    .unwrap();
    let app = TestApp::new(pool);

    let (found, total) = search(&app, "hobbit", "&availability=available").await;
    assert_eq!((found, total), (vec![f.hobbit_cookbook], 1));
    let (found, total) = search(&app, "hobbit", "&availability=on_loan").await;
    assert_eq!((found, total), (vec![f.hobbit], 1));
    let (found, total) = search(&app, "hobbit", "&availability=all").await;
    assert_eq!(
        (found, total),
        (sorted(vec![f.hobbit, f.hobbit_cookbook]), 2)
    );
    // The fallback path combines the same way.
    let (found, total) = search(&app, "zo", "&availability=on_loan").await;
    assert_eq!((found, total), (vec![], 0));
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn index_follows_updates_and_deletes(pool: SqlitePool) {
    let f = fixture(&pool).await;
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;

    // Update through the API: the old title stops matching, the new one starts.
    let body = json!({
        "title": "Arrakis Chronicles", "author": "Frank Herbert", "isbn": null,
        "published_year": 1965, "total_copies": 1,
    });
    let (status, _, json) = app
        .put_json(&format!("/api/books/{}", f.dune), &body, Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::OK, "{json}");
    assert_finds(&app, "Dune", vec![]).await;
    assert_finds(&app, "arrakis", vec![f.dune]).await;
    // Author unchanged, still found; the dropped ISBN no longer is.
    assert_finds(&app, "herbert", vec![f.dune]).await;
    assert_finds(&app, "0441172719", vec![]).await;

    // Update with plain SQL: the triggers do not care who writes.
    sqlx::query("UPDATE books SET author = $1 WHERE id = $2")
        .bind("Someone Else")
        .bind(f.dune)
        .execute(&pool)
        .await
        .unwrap();
    assert_finds(&app, "herbert", vec![]).await;
    assert_finds(&app, "someone else", vec![f.dune]).await;

    // Delete through the API.
    let (status, _, _) = app
        .delete(&format!("/api/books/{}", f.dune), Some(&cookie))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert_finds(&app, "arrakis", vec![]).await;
    assert_finds(&app, "someone else", vec![]).await;

    // Delete and insert with plain SQL.
    sqlx::query("DELETE FROM books WHERE id = $1")
        .bind(f.fellowship)
        .execute(&pool)
        .await
        .unwrap();
    assert_finds(&app, "fellowship", vec![]).await;
    assert_finds(&app, "tolkien", vec![f.hobbit]).await;
    let new_id = insert_book(&pool, "Neuromancer", "William Gibson", None).await;
    assert_finds(&app, "neuromancer", vec![new_id]).await;
}

#[sqlx::test(migrations = false)]
async fn books_from_before_the_migration_are_found_after_it(pool: SqlitePool) {
    // The schema as the previous release left it: everything up to the change log.
    let mut previous = sqlx::migrate!("../../migrations/sqlite");
    let migrations = previous
        .iter()
        .filter(|m| m.version <= 20261011000000)
        .cloned()
        .collect();
    previous.migrations = std::borrow::Cow::Owned(migrations);
    previous.run(&pool).await.unwrap();
    let has_index: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'books_fts')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!has_index, "the search index must not exist yet");

    let f = fixture(&pool).await;

    // Now the full set, as `library-web` runs it at startup. It also proves the
    // bundled SQLite has FTS5 and the trigram tokenizer.
    library_web::db::migrate(&pool).await.unwrap();

    let app = TestApp::new(pool);
    assert_finds(&app, "hobbit", vec![f.hobbit, f.hobbit_cookbook]).await;
    assert_finds(&app, "tolkien", vec![f.hobbit, f.fellowship]).await;
    assert_finds(&app, "978-0-261-10221-7", vec![f.hobbit]).await;
    assert_finds(&app, "Café", vec![f.cafe]).await;
}

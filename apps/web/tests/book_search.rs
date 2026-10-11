//! Book search (`GET /api/books?q=`): word prefixes (every word of `q` starts some
//! word of title, author or ISBN), the plain-text fallback for a `q` with no word,
//! and the index staying in sync with `books`.
//!
//! Raw SQL goes through `TestApp::scalar` with `$1` placeholders, so every test
//! runs on SQLite and on Postgres (`db_tests!`). Only the migration test is SQLite-only.
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

async fn insert_book(app: &TestApp, title: &str, author: &str, isbn: Option<&str>) -> i64 {
    app.scalar(
        "INSERT INTO books (title, author, isbn, total_copies, created_at, updated_at) \
         VALUES ($1, $2, $3, 1, 0, 0) RETURNING id",
        &[title.into(), author.into(), isbn.map(str::to_owned).into()],
    )
    .await
}

async fn fixture(app: &TestApp) -> Fixture {
    Fixture {
        hobbit: insert_book(app, "The Hobbit", "J.R.R. Tolkien", Some("9780261102217")).await,
        dune: insert_book(app, "Dune", "Frank Herbert", Some("9780441172719")).await,
        hobbit_cookbook: insert_book(
            app,
            "Hobbit Hole Cookbook",
            "Sam Gamgee",
            Some("9781234567897"),
        )
        .await,
        fellowship: insert_book(app, "The Fellowship of the Ring", "J.R.R. Tolkien", None).await,
        pure_joy: insert_book(app, "100% Pure_Joy", "Ann Author", None).await,
        hello: insert_book(app, "Say \"Hello\" World", "Bob Writer", None).await,
        cafe: insert_book(app, "Café Société", "Zoë Müller", None).await,
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

db_tests! {
async fn words_match_by_prefix_ignoring_case(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    assert_finds(&app, "hob", vec![f.hobbit, f.hobbit_cookbook]).await;
    assert_finds(&app, "HoBbIt", vec![f.hobbit, f.hobbit_cookbook]).await;
    assert_finds(&app, "tolk", vec![f.hobbit, f.fellowship]).await;
    assert_finds(&app, "GAM", vec![f.hobbit_cookbook]).await;
    // From 1 character on, in every column.
    assert_finds(&app, "d", vec![f.dune]).await;
    assert_finds(&app, "h", vec![f.hobbit, f.hobbit_cookbook, f.dune, f.hello]).await;
    // A prefix of the whole word works too, and an extension of it does not.
    assert_finds(&app, "dune", vec![f.dune]).await;
    assert_finds(&app, "dunes", vec![]).await;
}

async fn the_middle_of_a_word_does_not_match(db: Database) {
    let app = TestApp::new(db);
    fixture(&app).await;
    assert_finds(&app, "obbit", vec![]).await;
    assert_finds(&app, "olkien", vec![]).await;
    assert_finds(&app, "ellowship", vec![]).await;
    // "Fancy" contains "anc" but starts no word; "Ancient" starts with it.
    let fancy = insert_book(&app, "Fancy Dress", "Zed", None).await;
    assert_finds(&app, "anc", vec![]).await;
    let ancient = insert_book(&app, "Ancient Dress", "Zed", None).await;
    assert_finds(&app, "anc", vec![ancient]).await;
    assert_finds(&app, "fanc", vec![fancy]).await;
    assert_finds(&app, "ncy", vec![]).await;
    assert_finds(&app, "gee", vec![]).await;
}

async fn every_word_must_match_a_prefix(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    // Words may sit in different columns and in any order.
    assert_finds(&app, "hobbit tolkien", vec![f.hobbit]).await;
    assert_finds(&app, "tolkien hobbit", vec![f.hobbit]).await;
    assert_finds(&app, "the hobbit", vec![f.hobbit]).await;
    assert_finds(&app, "fellowship of the", vec![f.fellowship]).await;
    assert_finds(&app, "ring fellow", vec![f.fellowship]).await;
    assert_finds(&app, "hob cook", vec![f.hobbit_cookbook]).await;
    assert_finds(&app, "tolkien the", vec![f.hobbit, f.fellowship]).await;
    // Extra whitespace and punctuation between words change nothing.
    assert_finds(&app, "  hob ,  tolk.  ", vec![f.hobbit]).await;
    assert_finds(&app, "j.r.r. tolkien", vec![f.hobbit, f.fellowship]).await;
    assert_finds(&app, "hobbit herbert", vec![]).await;
    assert_finds(&app, "hobbit nonsense", vec![]).await;
}

async fn isbn_matches_by_prefix_typed_with_hyphens_and_spaces(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    assert_finds(&app, "97802611", vec![f.hobbit]).await;
    assert_finds(&app, "9780", vec![f.hobbit, f.dune]).await;
    assert_finds(&app, "9", vec![f.hobbit, f.dune, f.hobbit_cookbook]).await;
    // Stored without separators; the typed form has them, and is a prefix.
    assert_finds(&app, "978-0-261 10221-7", vec![f.hobbit]).await;
    assert_finds(&app, "978-0-441", vec![f.dune]).await;
    assert_finds(&app, "978 1", vec![f.hobbit_cookbook]).await;
    // Only a prefix: the middle of an ISBN does not match.
    assert_finds(&app, "0261102", vec![]).await;
    assert_finds(&app, "0441 172-719", vec![]).await;
    assert_finds(&app, "000-000", vec![]).await;
    // The check digit X, in either case.
    let tenth = insert_book(&app, "Tenth", "Ann", Some("012345678X")).await;
    assert_finds(&app, "012345678x", vec![tenth]).await;
    assert_finds(&app, "0-12345-678x", vec![tenth]).await;
    assert_finds(&app, "0-12345-678X", vec![tenth]).await;
}

async fn punctuation_and_search_syntax_are_literal_and_safe(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    // Punctuation only separates words (`%` and `_` are not LIKE wildcards).
    assert_finds(&app, "100%", vec![f.pure_joy]).await;
    assert_finds(&app, "Pure%Joy", vec![f.pure_joy]).await;
    assert_finds(&app, "Pur_ Joy", vec![f.pure_joy]).await;
    assert_finds(&app, "e_Joy", vec![]).await;
    assert_finds(&app, "\"Hello\"", vec![f.hello]).await;
    assert_finds(&app, "\"Hel", vec![f.hello]).await;
    assert_finds(&app, "say \"H", vec![f.hello]).await;
    assert_finds(&app, "(Dune)", vec![f.dune]).await;
    assert_finds(&app, "-Dune-", vec![f.dune]).await;
    assert_finds(&app, "Dun*", vec![f.dune]).await;
    // Query-language words and operators are ordinary words that match nothing here.
    assert_finds(&app, "title:Dune", vec![]).await;
    assert_finds(&app, "Dune OR Hobbit", vec![]).await;
    assert_finds(&app, "dune AND herbert NOT x", vec![]).await;
    assert_finds(&app, "NEAR(dune herbert)", vec![]).await;
    assert_finds(&app, "Dune NEAR Herbert", vec![]).await;
    assert_finds(&app, "\"*:(-NEAR \"\"", vec![]).await;
    assert_finds(&app, "'; DROP TABLE books; --", vec![]).await;
    assert_finds(&app, "dune & herbert | !x <-> y:*", vec![]).await;
    assert_finds(&app, "d\\", vec![f.dune]).await;
    // ... and they match as words when the book has them.
    let words = insert_book(&app, "Not And Or Near", "Ann Not", None).await;
    assert_finds(&app, "not or", vec![words]).await;
    assert_finds(&app, "near and not", vec![words]).await;
    assert_finds(&app, "OR", vec![words]).await;
    // With no word in `q` at all it is matched as plain text, like a substring.
    assert_finds(&app, "\"", vec![f.hello]).await;
    assert_finds(&app, "%", vec![f.pure_joy]).await;
    assert_finds(&app, "_", vec![f.pure_joy]).await;
    assert_finds(&app, "%%%", vec![]).await;
    assert_finds(&app, "-", vec![]).await;
    assert_finds(&app, "*", vec![]).await;
    assert_finds(&app, ":", vec![]).await;
    assert_finds(&app, "(", vec![]).await;
    assert_finds(&app, "\\", vec![]).await;
    assert_finds(&app, "\"\"\"", vec![]).await;
}

async fn non_ascii_words_in_the_stored_case(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    // Same case as stored: how other-case non-ASCII text folds depends on the
    // backend (SQLite folds all Unicode, Postgres by its locale).
    assert_finds(&app, "Café", vec![f.cafe]).await;
    assert_finds(&app, "Caf", vec![f.cafe]).await;
    assert_finds(&app, "Soc", vec![f.cafe]).await;
    assert_finds(&app, "Société", vec![f.cafe]).await;
    assert_finds(&app, "Müller", vec![f.cafe]).await;
    assert_finds(&app, "Mül", vec![f.cafe]).await;
    assert_finds(&app, "zoë", vec![f.cafe]).await;
    assert_finds(&app, "ZOË", vec![f.cafe]).await;
    // Accents stay significant on both backends.
    assert_finds(&app, "cafe", vec![]).await;
    assert_finds(&app, "Muller", vec![]).await;
    assert_finds(&app, "zoe", vec![]).await;
}

async fn combined_with_availability(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
    // The only copy of "The Hobbit" is out.
    let member_id = app
        .scalar(
            "INSERT INTO members (name, email, created_at, updated_at) \
             VALUES ('M', 'm@example.com', 0, 0) RETURNING id",
            &[],
        )
        .await;
    app.scalar(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1, $2, 0, 100) RETURNING id",
        &[f.hobbit.into(), member_id.into()],
    )
    .await;

    let (found, total) = search(&app, "hobbit", "&availability=available").await;
    assert_eq!((found, total), (vec![f.hobbit_cookbook], 1));
    let (found, total) = search(&app, "hobbit", "&availability=on_loan").await;
    assert_eq!((found, total), (vec![f.hobbit], 1));
    let (found, total) = search(&app, "hobbit", "&availability=all").await;
    assert_eq!(
        (found, total),
        (sorted(vec![f.hobbit, f.hobbit_cookbook]), 2)
    );
    let (found, total) = search(&app, "zo", "&availability=on_loan").await;
    assert_eq!((found, total), (vec![], 0));
    let (found, total) = search(&app, "\"", "&availability=available").await;
    assert_eq!((found, total), (vec![f.hello], 1));
}

async fn index_follows_updates_and_deletes(db: Database) {
    let app = TestApp::new(db);
    let f = fixture(&app).await;
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
    app.scalar(
        "UPDATE books SET author = $1 WHERE id = $2 RETURNING id",
        &["Someone Else".into(), f.dune.into()],
    )
    .await;
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
    app.scalar(
        "DELETE FROM books WHERE id = $1 RETURNING id",
        &[f.fellowship.into()],
    )
    .await;
    assert_finds(&app, "fellowship", vec![]).await;
    assert_finds(&app, "tolkien", vec![f.hobbit]).await;
    let new_id = insert_book(&app, "Neuromancer", "William Gibson", None).await;
    assert_finds(&app, "neuromancer", vec![new_id]).await;
}
}

#[sqlx::test(migrations = false)]
async fn books_from_before_the_migration_are_found_after_it(pool: SqlitePool) {
    // SQLite-only: it is about the SQLite migration set (FTS5 index added over existing rows).
    let app = TestApp::new(pool.clone());
    // The schema as the previous release left it: everything up to the change log.
    migrate_up_to(&pool, 20261011000000).await;
    let has_index: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'books_fts')")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!has_index, "the search index must not exist yet");

    let f = fixture(&app).await;

    // Now the full set, as `library-web` runs it at startup. It also proves the
    // bundled SQLite has FTS5 and the unicode61 tokenizer.
    library_web::db::migrate(&pool.clone().into())
        .await
        .unwrap();

    assert_finds(&app, "hobbit", vec![f.hobbit, f.hobbit_cookbook]).await;
    assert_finds(&app, "tolkien", vec![f.hobbit, f.fellowship]).await;
    assert_finds(&app, "978-0-261-10221-7", vec![f.hobbit]).await;
    assert_finds(&app, "Café", vec![f.cafe]).await;
}

#[sqlx::test(migrations = false)]
async fn the_prefix_index_replaces_the_trigram_index_over_existing_books(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    // The previous release: a trigram index kept in sync by its triggers.
    migrate_up_to(&pool, 20261012000000).await;
    let f = fixture(&app).await;
    assert_eq!(
        search(&app, "obbit", "").await.0,
        sorted(vec![f.hobbit, f.hobbit_cookbook]),
        "trigram: substring"
    );

    library_web::db::migrate(&pool.clone().into())
        .await
        .unwrap();

    // Existing rows are in the new index; the substring no longer matches.
    assert_finds(&app, "obbit", vec![]).await;
    assert_finds(&app, "hob", vec![f.hobbit, f.hobbit_cookbook]).await;
    // The triggers were recreated for the new table.
    let new_id = insert_book(&app, "Neuromancer", "William Gibson", None).await;
    assert_finds(&app, "neuro", vec![new_id]).await;
    app.scalar(
        "DELETE FROM books WHERE id = $1 RETURNING id",
        &[new_id.into()],
    )
    .await;
    assert_finds(&app, "neuro", vec![]).await;
}

/// Apply only the migrations up to and including `version`.
async fn migrate_up_to(pool: &SqlitePool, version: i64) {
    let mut migrator = sqlx::migrate!("../../migrations/sqlite");
    let migrations = migrator
        .iter()
        .filter(|m| m.version <= version)
        .cloned()
        .collect();
    migrator.migrations = std::borrow::Cow::Owned(migrations);
    migrator.run(pool).await.unwrap();
}

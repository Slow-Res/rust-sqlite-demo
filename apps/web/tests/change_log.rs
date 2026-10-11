//! The change log (spec §6.1): every committed write to a synced table leaves
//! exactly one row per changed row id, written by triggers in the same
//! transaction.
#![allow(clippy::unwrap_used)]

mod common;

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use common::TestApp;
use serde_json::json;
use sqlx::SqlitePool;

/// `(table_name, row_id, op)` for every change_log row, oldest first.
async fn log(pool: &SqlitePool) -> Vec<(String, i64, String)> {
    sqlx::query_as("SELECT table_name, row_id, op FROM change_log ORDER BY seq")
        .fetch_all(pool)
        .await
        .unwrap()
}

fn entry(table: &str, id: i64, op: &str) -> (String, i64, String) {
    (table.to_owned(), id, op.to_owned())
}

async fn insert_book(pool: &SqlitePool) -> i64 {
    sqlx::query("INSERT INTO books (title, author, isbn, published_year, total_copies, created_at, updated_at) VALUES ('T', 'A', NULL, NULL, 1, 0, 0)")
        .execute(pool).await.unwrap().last_insert_rowid()
}

async fn insert_member(pool: &SqlitePool, email: &str) -> i64 {
    sqlx::query("INSERT INTO members (name, email, created_at, updated_at) VALUES ('M', ?, 0, 0)")
        .bind(email)
        .execute(pool)
        .await
        .unwrap()
        .last_insert_rowid()
}

async fn insert_loan(pool: &SqlitePool, book_id: i64, member_id: i64) -> i64 {
    sqlx::query("INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES (?, ?, 0, 100, NULL)")
        .bind(book_id).bind(member_id).execute(pool).await.unwrap().last_insert_rowid()
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn book_insert_update_delete_are_logged(pool: SqlitePool) {
    let id = insert_book(&pool).await;
    sqlx::query("UPDATE books SET title = 'U' WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM books WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        log(&pool).await,
        vec![
            entry("books", id, "insert"),
            entry("books", id, "update"),
            entry("books", id, "delete")
        ]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn member_and_loan_writes_are_logged(pool: SqlitePool) {
    let book = insert_book(&pool).await;
    let member = insert_member(&pool, "a@example.com").await;
    let loan = insert_loan(&pool, book, member).await;
    sqlx::query("UPDATE members SET name = 'N' WHERE id = ?")
        .bind(member)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE loans SET returned_at = 50 WHERE id = ?")
        .bind(loan)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM loans WHERE id = ?")
        .bind(loan)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        log(&pool).await,
        vec![
            entry("books", book, "insert"),
            entry("members", member, "insert"),
            entry("loans", loan, "insert"),
            entry("members", member, "update"),
            entry("loans", loan, "update"),
            entry("loans", loan, "delete"),
        ]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn cascaded_loan_deletes_are_logged(pool: SqlitePool) {
    let book = insert_book(&pool).await;
    let member = insert_member(&pool, "a@example.com").await;
    let loan = insert_loan(&pool, book, member).await;
    sqlx::query("DELETE FROM change_log")
        .execute(&pool)
        .await
        .unwrap();
    // loans.book_id is ON DELETE CASCADE: deleting the book deletes the loan.
    sqlx::query("DELETE FROM books WHERE id = ?")
        .bind(book)
        .execute(&pool)
        .await
        .unwrap();
    let mut entries = log(&pool).await;
    entries.sort();
    assert_eq!(
        entries,
        vec![
            entry("books", book, "delete"),
            entry("loans", loan, "delete")
        ]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn changing_an_id_logs_the_old_id_as_deleted(pool: SqlitePool) {
    let id = insert_book(&pool).await;
    sqlx::query("DELETE FROM change_log")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE books SET id = 99 WHERE id = ?")
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        log(&pool).await,
        vec![entry("books", 99, "update"), entry("books", id, "delete")]
    );
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn users_and_sessions_are_not_logged(pool: SqlitePool) {
    let user =
        sqlx::query("INSERT INTO users (username, password_hash, created_at) VALUES ('u', 'h', 0)")
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid();
    sqlx::query(
        "INSERT INTO sessions (token_hash, user_id, created_at, expires_at) VALUES ('t', ?, 0, 10)",
    )
    .bind(user)
    .execute(&pool)
    .await
    .unwrap();
    assert!(log(&pool).await.is_empty());
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn each_entry_has_a_random_token_and_a_millisecond_time(pool: SqlitePool) {
    let now_ms = || {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let before = now_ms();
    insert_book(&pool).await;
    insert_book(&pool).await;
    let after = now_ms();
    let rows: Vec<(Vec<u8>, i64)> =
        sqlx::query_as("SELECT token, created_at FROM change_log ORDER BY seq")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0.len(), 8);
    assert_ne!(rows[0].0, rows[1].0, "tokens are random");
    for (_, created_at) in rows {
        assert!(
            (before..=after).contains(&created_at),
            "{before} <= {created_at} <= {after}"
        );
    }
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn sequence_numbers_are_never_reused(pool: SqlitePool) {
    insert_book(&pool).await;
    let first: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM change_log")
        .execute(&pool)
        .await
        .unwrap();
    insert_book(&pool).await;
    let second: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(second > first, "AUTOINCREMENT must not reuse {first}");
}

#[sqlx::test(migrations = "../../migrations/sqlite")]
async fn creating_a_book_through_the_api_logs_one_insert(pool: SqlitePool) {
    let app = TestApp::new(pool.clone());
    let cookie = app.login("ada", "password123").await;
    let body = json!({ "title": "Dune", "author": "Frank Herbert", "isbn": null, "published_year": null, "total_copies": 1 });
    let (status, _, book) = app.post_json("/api/books", &body, Some(&cookie)).await;
    assert_eq!(status, StatusCode::CREATED, "{book}");
    assert_eq!(
        log(&pool).await,
        vec![entry("books", book["id"].as_i64().unwrap(), "insert")]
    );
}

//! SMTP replies decide retry vs permanent. One test only: it switches Mailpit's
//! global chaos triggers, which would disturb any test running beside it.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::db::Database;
use library_web::jobs::events::LoanEventKind;
use library_web::jobs::loan_email::{LoanEmail, Mailer};
use serde_json::json;

fn var(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is not set: eval \"$(./scripts/test-mailpit.sh)\""))
}

async fn chaos(recipient_code: u16, probability: u8) {
    let body = json!({ "Recipient": { "ErrorCode": recipient_code, "Probability": probability } });
    reqwest::Client::new()
        .put(format!("{}/api/v1/chaos", var("TEST_MAILPIT_URL")))
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn smtp_5xx_is_permanent_and_4xx_is_retry() {
    common::on_postgres(|db| async move {
        chaos(0, 0).await; // a killed earlier run may have left chaos on
        let Database::Postgres(pool) = db else { panic!("postgres only") };
        let member: i64 = sqlx::query_scalar("INSERT INTO members (name, email, created_at, updated_at) VALUES ('C', 'chaos@example.test', 0, 0) RETURNING id")
            .fetch_one(&pool).await.unwrap();
        let book: i64 = sqlx::query_scalar("INSERT INTO books (title, author, total_copies, created_at, updated_at) VALUES ('T', 'A', 1, 0, 0) RETURNING id")
            .fetch_one(&pool).await.unwrap();
        let loan_id: i64 = sqlx::query_scalar("INSERT INTO loans (book_id, member_id, borrowed_at, due_at) VALUES ($1, $2, 0, 100) RETURNING id")
            .bind(book).bind(member).fetch_one(&pool).await.unwrap();
        let send = move || {
            let mut deps = Deps::new();
            deps.insert(Arc::new(Mailer::new(&var("TEST_SMTP_URL"), "l@library.test", "library.test", Duration::from_secs(10)).unwrap()));
            LoanEmail { loan_id, event: LoanEventKind::Borrowed }.perform(JobContext::detached(pool.clone(), 9, 1, LoanEmail::KIND, deps))
        };
        let outcome = tokio::spawn(async move {
            chaos(550, 100).await;
            let permanent = send().await;
            chaos(451, 100).await;
            let retry = send().await;
            (permanent, retry)
        })
        .await;
        chaos(0, 0).await; // always switch chaos off again
        let (permanent, retry) = outcome.unwrap();
        assert!(matches!(permanent, Err(JobError::Permanent(_))), "{permanent:?}");
        assert!(matches!(retry, Err(JobError::Retry(_))), "{retry:?}");
    })
    .await;
}

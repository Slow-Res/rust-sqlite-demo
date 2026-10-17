//! `loan_email.v1` against Mailpit (`eval "$(./scripts/test-mailpit.sh)"`).
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::db::Database;
use library_web::jobs::events::LoanEventKind;
use library_web::jobs::loan_email::{LoanEmail, Mailer};
use serde_json::Value;
use sqlx::PgPool;

pub fn smtp_url() -> String {
    std::env::var("TEST_SMTP_URL")
        .expect("TEST_SMTP_URL is not set: eval \"$(./scripts/test-mailpit.sh)\"")
}

pub fn mailpit_url() -> String {
    std::env::var("TEST_MAILPIT_URL")
        .expect("TEST_MAILPIT_URL is not set: eval \"$(./scripts/test-mailpit.sh)\"")
}

fn pg(db: Database) -> PgPool {
    match db {
        Database::Postgres(pool) => pool,
        _ => panic!("postgres only"),
    }
}

/// A member with an address no other test uses (Mailpit is shared), a book and a loan.
async fn loan(pool: &PgPool, email: Option<&str>, returned: bool) -> (i64, String) {
    let email = email.map_or_else(
        || format!("m{}@example.test", rand::random::<u64>()),
        str::to_owned,
    );
    let member: i64 = sqlx::query_scalar("INSERT INTO members (name, email, created_at, updated_at) VALUES ('Ann Reader', $1, 0, 0) RETURNING id")
        .bind(&email).fetch_one(pool).await.unwrap();
    let book: i64 = sqlx::query_scalar("INSERT INTO books (title, author, total_copies, created_at, updated_at) VALUES ('Dune', 'Frank Herbert', 1, 0, 0) RETURNING id")
        .fetch_one(pool).await.unwrap();
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO loans (book_id, member_id, borrowed_at, due_at, returned_at) VALUES ($1, $2, 1800000000, 1801209600, $3) RETURNING id",
    ).bind(book).bind(member).bind(returned.then_some(1_800_100_000_i64)).fetch_one(pool).await.unwrap();
    (id, email)
}

fn ctx(pool: &PgPool, job_id: i64, mailer: Mailer) -> JobContext {
    let mut deps = Deps::new();
    deps.insert(Arc::new(mailer));
    JobContext::detached(pool.clone(), job_id, 1, LoanEmail::KIND, deps)
}

fn mailer(url: &str, timeout: Duration) -> Mailer {
    Mailer::new(
        url,
        "Library <library@library.test>",
        "library.test",
        timeout,
    )
    .unwrap()
}

/// Messages Mailpit holds for `to`, waiting up to 5 s for `want` of them.
async fn inbox(to: &str, want: usize) -> Vec<Value> {
    let url = reqwest::Url::parse_with_params(
        &format!("{}/api/v1/search", mailpit_url()),
        &[("query", format!("to:{to}"))],
    )
    .unwrap();
    let start = Instant::now();
    loop {
        let body: Value = reqwest::get(url.clone())
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let messages = body["messages"].as_array().cloned().unwrap_or_default();
        if messages.len() >= want || start.elapsed() > Duration::from_secs(5) {
            return messages;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_resend_carries_the_same_message_id() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, to) = loan(&pool, None, false).await;
        let job = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        };
        // Twice, as after a crash between the server's 250 and our completion write.
        job.clone()
            .perform(ctx(
                &pool,
                4242,
                mailer(&smtp_url(), Duration::from_secs(10)),
            ))
            .await
            .unwrap();
        job.perform(ctx(
            &pool,
            4242,
            mailer(&smtp_url(), Duration::from_secs(10)),
        ))
        .await
        .unwrap();
        let messages = inbox(&to, 2).await;
        assert_eq!(messages.len(), 2);
        for m in &messages {
            assert_eq!(
                m["MessageID"].as_str().unwrap().trim_matches(['<', '>']),
                "loan_email.v1.4242@library.test"
            );
            assert_eq!(m["Subject"], "You borrowed \"Dune\"");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_sent_for_a_deleted_or_already_returned_loan() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let gone = LoanEmail {
            loan_id: 999_999,
            event: LoanEventKind::Borrowed,
        };
        gone.perform(ctx(&pool, 1, mailer(&smtp_url(), Duration::from_secs(10))))
            .await
            .unwrap();
        let (loan_id, to) = loan(&pool, None, true).await;
        let stale = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        };
        stale
            .perform(ctx(&pool, 2, mailer(&smtp_url(), Duration::from_secs(10))))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(inbox(&to, 0).await.is_empty());
        let returned = LoanEmail {
            loan_id,
            event: LoanEventKind::Returned,
        };
        returned
            .perform(ctx(&pool, 3, mailer(&smtp_url(), Duration::from_secs(10))))
            .await
            .unwrap();
        assert_eq!(inbox(&to, 1).await[0]["Subject"], "You returned \"Dune\"");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_member_address_is_permanent() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, Some("not-an-address"), false).await;
        let err = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        }
        .perform(ctx(&pool, 5, mailer(&smtp_url(), Duration::from_secs(10))))
        .await
        .unwrap_err();
        assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
        // The stored error must not hold the member's data.
        let text = format!("{err} {err:?}");
        for personal in ["not-an-address", "Ann Reader"] {
            assert!(!text.contains(personal), "`{personal}` leaked into: {text}");
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn hanging_smtp_server_times_out_as_retry() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, None, false).await;
        // Accepts connections and never says a word.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let start = Instant::now();
        let err = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        }
        .perform(ctx(
            &pool,
            6,
            mailer(&format!("smtp://127.0.0.1:{port}"), Duration::from_secs(1)),
        ))
        .await
        .unwrap_err();
        assert!(matches!(err, JobError::Retry(_)), "{err:?}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_connection_is_retry() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, None, false).await;
        let err = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        }
        .perform(ctx(
            &pool,
            7,
            mailer("smtp://127.0.0.1:1", Duration::from_secs(2)),
        ))
        .await
        .unwrap_err();
        assert!(matches!(err, JobError::Retry(_)), "{err:?}");
    })
    .await;
}

/// The SMTP URL may carry a password; neither a failed send nor a bad URL may
/// put it, or the server address, into an error that is stored and logged.
#[tokio::test(flavor = "multi_thread")]
async fn smtp_credentials_never_reach_error_text() {
    common::on_postgres(|db| async move {
        let pool = pg(db);
        let (loan_id, _) = loan(&pool, None, false).await;
        let err = LoanEmail {
            loan_id,
            event: LoanEventKind::Borrowed,
        }
        .perform(ctx(
            &pool,
            8,
            mailer(
                "smtp://mailer:hunter2secret@127.0.0.1:1?tls=required",
                Duration::from_secs(2),
            ),
        ))
        .await
        .unwrap_err();
        let text = format!("{err} {err:?}");
        assert!(matches!(err, JobError::Retry(_)), "{text}");
        for secret in ["hunter2secret", "mailer:", "127.0.0.1"] {
            assert!(!text.contains(secret), "`{secret}` leaked into: {text}");
        }
    })
    .await;
    let Err(err) = Mailer::new(
        "smtp://mailer:hunter2secret@bad host:99999999?tls=required",
        "l@library.test",
        "library.test",
        Duration::from_secs(1),
    ) else {
        panic!("a malformed SMTP URL must be refused");
    };
    let text = format!("{err:#} {err:?}");
    assert!(
        !text.contains("hunter2secret"),
        "password leaked into: {text}"
    );
}

/// A server that speaks SMTP up to `354` of DATA. The first connection then goes
/// silent (the final reply never comes); later ones refuse the recipient (550).
/// Returns its port and the number of connections it has accepted.
async fn stalling_then_refusing_server() -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    tokio::spawn(async move {
        while let Ok((socket, _)) = listener.accept().await {
            let nth = count.fetch_add(1, Ordering::SeqCst) + 1;
            tokio::spawn(async move {
                let (read, mut write) = socket.into_split();
                let mut lines = BufReader::new(read).lines();
                write.write_all(b"220 fake ESMTP\r\n").await.unwrap();
                let mut in_data = false;
                while let Ok(Some(line)) = lines.next_line().await {
                    let upper = line.to_ascii_uppercase();
                    let reply: &[u8] = if in_data {
                        if line == "." {
                            // End of the message: the first connection never answers.
                            if nth == 1 {
                                std::future::pending::<()>().await;
                            }
                            in_data = false;
                            b"250 queued\r\n"
                        } else {
                            continue;
                        }
                    } else if upper.starts_with("EHLO") {
                        b"250 fake\r\n"
                    } else if upper.starts_with("MAIL") {
                        b"250 ok\r\n"
                    } else if upper.starts_with("RCPT") {
                        if nth == 1 {
                            b"250 ok\r\n"
                        } else {
                            b"550 no such user\r\n"
                        }
                    } else if upper.starts_with("DATA") {
                        in_data = true;
                        b"354 go on\r\n"
                    } else if upper.starts_with("QUIT") {
                        b"221 bye\r\n"
                    } else {
                        b"250 ok\r\n"
                    };
                    if write.write_all(reply).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    (port, accepted)
}

/// A send cancelled by the timeout after DATA must not leave a connection that the
/// next send reuses: its reply would be the old message's (a stale 250 would turn a
/// failed send into `Ok`). Every send opens its own connection.
#[tokio::test(flavor = "multi_thread")]
async fn a_send_cancelled_after_data_does_not_poison_the_next_one() {
    use std::sync::atomic::Ordering;

    let (port, accepted) = stalling_then_refusing_server().await;
    let mailer = mailer(&format!("smtp://127.0.0.1:{port}"), Duration::from_secs(1));
    let send = |job_id: i64| {
        let message = mailer
            .message(job_id, "Ann", "ann@example.test", "Hi", "Body\n".to_owned())
            .unwrap();
        mailer.send(message)
    };
    let first = send(1).await.unwrap_err();
    assert!(matches!(first, JobError::Retry(_)), "{first:?}");
    let second = send(2).await.unwrap_err();
    assert!(matches!(second, JobError::Permanent(_)), "{second:?}");
    assert_eq!(
        accepted.load(Ordering::SeqCst),
        2,
        "the second send reused a connection"
    );
}

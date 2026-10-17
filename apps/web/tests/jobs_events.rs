//! Borrow and return enqueue their jobs in the same transaction (spec §15.1).
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use common::TestApp;
use library_web::db::Database;
use library_web::jobs::webhook::endpoint_id;
use library_web::jobs_config::{JobsConfig, Secret, WebhookEndpoint};
use serde_json::{Value, json};
use sqlx::PgPool;
use tokio_util::sync::CancellationToken;

// Test key, local only.
const SECRET: &str = "whsec_AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=";

fn jobs_on(smtp: bool, endpoints: &[&str]) -> JobsConfig {
    JobsConfig {
        enabled: true,
        notify: false,
        smtp_url: smtp.then(|| Secret::new("smtp://127.0.0.1:1")),
        webhooks: endpoints
            .iter()
            .map(|u| WebhookEndpoint {
                url: (*u).to_owned(),
                secret: Secret::new(SECRET),
            })
            .collect(),
        ..JobsConfig::default()
    }
}

/// The app's schema is migrated by `on_postgres`; the jobs schema is added here.
async fn with_jobs_schema(db: &Database) -> PgPool {
    let Database::Postgres(pool) = db else {
        panic!("postgres only")
    };
    library_web::jobs::migrate(pool).await.unwrap();
    pool.clone()
}

async fn setup(app: &TestApp, copies: i64) -> (String, i64, i64) {
    let cookie = app.login("librarian", "correct horse battery").await;
    let (s, _, book) = app
        .post_json(
            "/api/books",
            &json!({ "title": "Dune", "author": "Herbert", "total_copies": copies }),
            Some(&cookie),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{book}");
    let (s, _, member) = app
        .post_json(
            "/api/members",
            &json!({ "name": "Ann", "email": "ann@example.com" }),
            Some(&cookie),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{member}");
    (
        cookie,
        book["id"].as_i64().unwrap(),
        member["id"].as_i64().unwrap(),
    )
}

async fn jobs(pool: &PgPool) -> Vec<(String, String, Value)> {
    sqlx::query_as("SELECT kind, queue, args FROM jobs ORDER BY id")
        .fetch_all(pool)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn borrow_and_return_enqueue_one_email_and_one_webhook_per_endpoint() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let (url_a, url_b) = ("https://a.example/h", "https://b.example/h");
        let app = TestApp::with_jobs(db, jobs_on(true, &[url_a, url_b]));
        let (cookie, book, member) = setup(&app, 1).await;
        let (s, _, loan) = app
            .post_json(
                "/api/loans",
                &json!({ "book_id": book, "member_id": member }),
                Some(&cookie),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        let id = loan["id"].as_i64().unwrap();

        let rows = jobs(&pool).await;
        assert_eq!(rows.len(), 3, "{rows:?}");
        assert_eq!(
            (rows[0].0.as_str(), rows[0].1.as_str()),
            ("loan_email.v1", "email")
        );
        assert_eq!(rows[0].2, json!({ "loan_id": id, "event": "borrowed" }));
        let mut endpoints: Vec<&str> = rows[1..]
            .iter()
            .map(|r| r.2["endpoint"].as_str().unwrap())
            .collect();
        endpoints.sort_unstable();
        let mut want = [endpoint_id(url_a), endpoint_id(url_b)];
        want.sort_unstable();
        assert_eq!(endpoints, want);
        for r in &rows[1..] {
            assert_eq!((r.0.as_str(), r.1.as_str()), ("webhook.v1", "webhooks"));
            assert_eq!(r.2["event"]["id"], format!("loan.{id}.borrowed"));
            assert_eq!(r.2["event"]["type"], "loan.borrowed");
            assert_eq!(r.2["event"]["data"]["loan"]["book_title"], "Dune");
        }
        // Job args are stored: no URL in them, only the endpoint ids.
        assert!(rows[1..].iter().all(|r| !r.2.to_string().contains("/h\"")));

        let (s, _, returned) = app
            .post_json(
                &format!("/api/loans/{id}/return"),
                &json!({}),
                Some(&cookie),
            )
            .await;
        assert_eq!(s, StatusCode::OK);
        let rows = jobs(&pool).await;
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[3].2, json!({ "loan_id": id, "event": "returned" }));
        assert!(
            rows[4..]
                .iter()
                .all(|r| r.2["event"]["type"] == "loan.returned")
        );
        // The event is stamped from the post-update row: the return time.
        assert!(returned["returned_at"].is_string(), "{returned}");
        for r in &rows[4..] {
            assert_eq!(r.2["event"]["occurred_at"], returned["returned_at"]);
            assert_eq!(
                r.2["event"]["data"]["loan"]["returned_at"],
                returned["returned_at"]
            );
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn duplicate_endpoint_entries_enqueue_one_webhook() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let url = "https://a.example/h";
        let app = TestApp::with_jobs(db, jobs_on(false, &[url, url]));
        let (cookie, book, member) = setup(&app, 1).await;
        let (s, _, _) = app
            .post_json(
                "/api/loans",
                &json!({ "book_id": book, "member_id": member }),
                Some(&cookie),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        let rows = jobs(&pool).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].2["endpoint"], endpoint_id(url));
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_borrow_enqueues_nothing() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::with_jobs(db, jobs_on(true, &["https://a.example/h"]));
        let (cookie, book, member) = setup(&app, 1).await;
        let body = json!({ "book_id": book, "member_id": member });
        assert_eq!(
            app.post_json("/api/loans", &body, Some(&cookie)).await.0,
            StatusCode::CREATED
        );
        assert_eq!(
            app.post_json("/api/loans", &body, Some(&cookie)).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            jobs(&pool).await.len(),
            2,
            "only the first borrow's email and webhook"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn with_jobs_disabled_nothing_is_enqueued() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::new(db);
        let (cookie, book, member) = setup(&app, 1).await;
        let (s, _, _) = app
            .post_json(
                "/api/loans",
                &json!({ "book_id": book, "member_id": member }),
                Some(&cookie),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED);
        assert!(jobs(&pool).await.is_empty());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_enqueue_rolls_the_borrow_back() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let app = TestApp::with_jobs(db, jobs_on(true, &[]));
        let (cookie, book, member) = setup(&app, 1).await;
        sqlx::query("DROP TABLE jobs CASCADE")
            .execute(&pool)
            .await
            .unwrap();
        let (s, _, _) = app
            .post_json(
                "/api/loans",
                &json!({ "book_id": book, "member_id": member }),
                Some(&cookie),
            )
            .await;
        assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
        let loans: i64 = sqlx::query_scalar("SELECT count(*) FROM loans")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(loans, 0, "no loan without its jobs");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_worker_delivers_the_borrow_webhook() {
    common::on_postgres(|db| async move {
        let pool = with_jobs_schema(&db).await;
        let seen: Arc<Mutex<Vec<String>>> = Arc::default();
        let receiver = {
            let seen = seen.clone();
            Router::new().fallback(move |headers: HeaderMap| {
                let seen = seen.clone();
                async move {
                    seen.lock()
                        .unwrap()
                        .push(headers["webhook-id"].to_str().unwrap().to_owned());
                    StatusCode::NO_CONTENT
                }
            })
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/hook", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, receiver).await.unwrap() });

        let config = jobs_on(false, &[url.as_str()]);
        let app = TestApp::with_jobs(db, config.clone());
        let mut worker_config = library_web::jobs::worker_config(&config, "test").unwrap();
        worker_config.poll = Duration::from_millis(100);
        let worker = library_jobs::Worker::new(
            pool.clone(),
            library_web::jobs::registry(&config).unwrap(),
            worker_config,
        )
        .unwrap();
        let stop = CancellationToken::new();
        let handle = tokio::spawn(worker.run(stop.clone()));

        let (cookie, book, member) = setup(&app, 1).await;
        let (_, _, loan) = app
            .post_json(
                "/api/loans",
                &json!({ "book_id": book, "member_id": member }),
                Some(&cookie),
            )
            .await;
        let want = format!("loan.{}.borrowed", loan["id"]);
        let start = Instant::now();
        while !seen.lock().unwrap().contains(&want) {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "webhook not delivered in 10 s"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        stop.cancel();
        handle.await.unwrap().unwrap();
        let state: String = sqlx::query_scalar("SELECT state FROM jobs WHERE kind = 'webhook.v1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(state, "succeeded");
    })
    .await;
}

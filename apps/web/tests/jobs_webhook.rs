//! `webhook.v1` against an in-test receiver. No database: the job context gets a
//! lazy pool that never connects.
#![cfg(feature = "postgres")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{TimeZone, Utc};
use library_jobs::{Deps, Job, JobContext, JobError};
use library_web::jobs::webhook::{Webhook, WebhookEvent, WebhookSender, endpoint_id, sign};
use library_web::jobs_config::{Secret, WebhookEndpoint};
use serde_json::{Value, json};

const KEY: [u8; 32] = [7; 32];

/// A canned reply: status and headers.
type Reply = (u16, Vec<(&'static str, String)>);

#[derive(Clone, Default)]
struct Receiver {
    hits: Arc<Mutex<Vec<(String, HeaderMap, Bytes)>>>,
    replies: Arc<Mutex<VecDeque<Reply>>>,
}

async fn hit(State(r): State<Receiver>, uri: Uri, headers: HeaderMap, body: Bytes) -> Response {
    r.hits
        .lock()
        .unwrap()
        .push((uri.path().to_owned(), headers, body));
    let (status, headers) = r
        .replies
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or((204, Vec::new()));
    let mut response = StatusCode::from_u16(status).unwrap().into_response();
    for (name, value) in headers {
        response.headers_mut().insert(name, value.parse().unwrap());
    }
    response
}

/// Start a receiver; returns it and its base URL.
async fn receiver() -> (Receiver, String) {
    let r = Receiver::default();
    let app = Router::new().fallback(hit).with_state(r.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (r, base)
}

fn endpoint(url: &str) -> WebhookEndpoint {
    WebhookEndpoint {
        url: url.to_owned(),
        secret: Secret::new(format!("whsec_{}", STANDARD.encode(KEY))),
    }
}

fn sender(urls: &[&str], timeout: Duration) -> WebhookSender {
    let endpoints: Vec<_> = urls.iter().map(|u| endpoint(u)).collect();
    WebhookSender::new(&endpoints, Duration::from_secs(2), timeout).unwrap()
}

fn event() -> WebhookEvent {
    WebhookEvent {
        id: "loan.7.borrowed".into(),
        kind: "loan.borrowed".into(),
        occurred_at: Utc.timestamp_opt(1_800_000_000, 0).unwrap(),
        data: json!({ "loan": { "id": 7 } }),
    }
}

#[tokio::test]
async fn a_delivered_event_is_signed_and_verifiable() {
    let (r, base) = receiver().await;
    let url = format!("{base}/hook");
    sender(&[url.as_str()], Duration::from_secs(5))
        .deliver(&endpoint_id(&url), &event())
        .await
        .unwrap();
    let hits = r.hits.lock().unwrap().clone();
    let (path, headers, body) = &hits[0];
    assert_eq!(path, "/hook");
    assert_eq!(headers["webhook-id"], "loan.7.borrowed");
    let ts: i64 = headers["webhook-timestamp"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        headers["webhook-signature"].to_str().unwrap(),
        sign(&KEY, "loan.7.borrowed", ts, body).unwrap()
    );
    let json: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        (json["id"].as_str(), json["type"].as_str()),
        (Some("loan.7.borrowed"), Some("loan.borrowed"))
    );
    assert_eq!(json["occurred_at"], "2027-01-15T08:00:00Z");
}

#[tokio::test]
async fn too_many_requests_honours_retry_after() {
    let (r, base) = receiver().await;
    r.replies
        .lock()
        .unwrap()
        .push_back((429, vec![("retry-after", "7".into())]));
    let err = sender(&[base.as_str()], Duration::from_secs(5))
        .deliver(&endpoint_id(&base), &event())
        .await
        .unwrap_err();
    assert!(
        matches!(err, JobError::RetryAfter(d, _) if d == Duration::from_secs(7)),
        "{err:?}"
    );
}

#[tokio::test]
async fn server_errors_retry_and_client_errors_are_permanent() {
    let (r, base) = receiver().await;
    let s = sender(&[base.as_str()], Duration::from_secs(5));
    for (status, retry) in [(500, true), (408, true), (404, false), (401, false)] {
        r.replies.lock().unwrap().push_back((status, Vec::new()));
        let err = s.deliver(&endpoint_id(&base), &event()).await.unwrap_err();
        assert_eq!(
            matches!(err, JobError::Retry(_)),
            retry,
            "{status}: {err:?}"
        );
        assert_eq!(
            matches!(err, JobError::Permanent(_)),
            !retry,
            "{status}: {err:?}"
        );
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let (r, base) = receiver().await;
    let url = format!("{base}/hook");
    r.replies
        .lock()
        .unwrap()
        .push_back((302, vec![("location", format!("{base}/elsewhere"))]));
    let err = sender(&[url.as_str()], Duration::from_secs(5))
        .deliver(&endpoint_id(&url), &event())
        .await
        .unwrap_err();
    assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
    let paths: Vec<String> = r.hits.lock().unwrap().iter().map(|h| h.0.clone()).collect();
    assert_eq!(paths, ["/hook"]);
}

#[tokio::test]
async fn hanging_endpoint_times_out_as_retry() {
    // Accepts connections and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((socket, _)) = listener.accept().await {
            held.push(socket);
        }
    });
    let start = Instant::now();
    let err = sender(&[url.as_str()], Duration::from_secs(1))
        .deliver(&endpoint_id(&url), &event())
        .await
        .unwrap_err();
    assert!(matches!(err, JobError::Retry(_)), "{err:?}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn an_endpoint_removed_from_config_is_permanent() {
    let err = sender(&["http://127.0.0.1:9/a"], Duration::from_secs(1))
        .deliver(&endpoint_id("http://127.0.0.1:9/b"), &event())
        .await
        .unwrap_err();
    assert!(matches!(err, JobError::Permanent(_)), "{err:?}");
}

#[tokio::test]
async fn perform_uses_the_provided_sender() {
    let (r, base) = receiver().await;
    let mut deps = Deps::new();
    deps.insert(Arc::new(sender(&[base.as_str()], Duration::from_secs(5))));
    // Never connects: the webhook job does not touch the database.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1:1/unused")
        .unwrap(); // local only
    let job = Webhook {
        endpoint: endpoint_id(&base),
        event: event(),
    };
    job.perform(JobContext::detached(pool, 1, 1, Webhook::KIND, deps))
        .await
        .unwrap();
    assert_eq!(r.hits.lock().unwrap().len(), 1);
}
#[test]
fn endpoint_ids_hide_everything_but_the_origin() {
    let id = endpoint_id("http://user:pw@example.com:8443/hooks/SECRET-TOKEN?k=SECRET-QUERY");
    let (origin, hash) = id.split_once('#').unwrap();
    assert_eq!(origin, "http://example.com:8443");
    assert_eq!(hash.len(), 12);
    assert!(hash.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(
        endpoint_id("http://example.com/a"),
        endpoint_id("http://example.com/b")
    );
    assert_eq!(
        endpoint_id("http://example.com/a"),
        endpoint_id("http://example.com/a")
    );
}

#[tokio::test]
async fn urls_with_tokens_never_reach_errors_args_or_keys() {
    let (r, base) = receiver().await;
    let port = base.rsplit(':').next().unwrap();
    let url = format!("http://user:pw@127.0.0.1:{port}/hooks/SECRET-TOKEN");
    let id = endpoint_id(&url);
    let s = sender(&[url.as_str()], Duration::from_secs(5));
    let job = Webhook {
        endpoint: id.clone(),
        event: event(),
    };

    // A refused connection: bind a port, then free it.
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let refused_url = format!("http://user:pw@127.0.0.1:{closed}/hooks/SECRET-TOKEN");
    let refused = sender(&[refused_url.as_str()], Duration::from_secs(2));

    r.replies.lock().unwrap().push_back((500, Vec::new()));
    r.replies.lock().unwrap().push_back((404, Vec::new()));
    let errors = vec![
        s.deliver(&id, &event()).await.unwrap_err(),
        s.deliver(&id, &event()).await.unwrap_err(),
        s.deliver(
            &endpoint_id("http://other.example/hooks/SECRET-TOKEN"),
            &event(),
        )
        .await
        .unwrap_err(),
        refused
            .deliver(&endpoint_id(&refused_url), &event())
            .await
            .unwrap_err(),
    ];
    assert!(matches!(errors[0], JobError::Retry(_)) && matches!(errors[1], JobError::Permanent(_)));
    assert!(matches!(errors[3], JobError::Retry(_)), "{:?}", errors[3]);
    for err in &errors {
        for text in [err.to_string(), format!("{err:?}")] {
            for secret in ["SECRET-TOKEN", "pw@", "user:pw", "/hooks"] {
                assert!(!text.contains(secret), "{secret} leaked into {text}");
            }
        }
    }
    let stored = serde_json::to_string(&job).unwrap() + &job.unique_key().unwrap();
    for secret in ["SECRET-TOKEN", "pw@", "user:pw", "/hooks"] {
        assert!(!stored.contains(secret), "{secret} leaked into {stored}");
    }
}

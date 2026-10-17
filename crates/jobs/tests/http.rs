#![cfg(feature = "http")]
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::*;
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn get(router: &axum::Router, path: &str) -> (StatusCode, String) {
    let res = router
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let body = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_heartbeats_and_metrics_count_jobs() {
    with_db(|pool| async move {
        let config = fast_config("default:2");
        let w = start(&pool, config.clone());
        let router = library_jobs::http::router(
            w.metrics.clone(),
            w.health.clone(),
            pool.clone(),
            config.heartbeat,
        );
        assert_eq!(get(&router, "/healthz").await.0, StatusCode::OK);
        eventually(Duration::from_secs(3), "ready", || async {
            get(&router, "/readyz").await.0 == StatusCode::OK
        })
        .await;
        for n in 0..3 {
            enqueue(&pool, &Noop { n }).await;
        }
        eventually(Duration::from_secs(5), "counted", || async {
            get(&router, "/metrics")
                .await
                .1
                .contains(r#"jobs_completed_total{kind="test_noop.v1",outcome="succeeded"} 3"#)
        })
        .await;
        sqlx::query("DELETE FROM job_processes")
            .execute(&pool)
            .await
            .unwrap();
        eventually(Duration::from_secs(3), "not ready once fenced", || async {
            let (status, body) = get(&router, "/readyz").await;
            status == StatusCode::SERVICE_UNAVAILABLE && body.contains("self-fenced")
        })
        .await;
        let _ = w.handle.await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn worker_router_serves_the_same_endpoints() {
    with_db(|pool| async move {
        let worker =
            library_jobs::Worker::new(pool.clone(), registry(), fast_config("default:1")).unwrap();
        let router = worker.router();
        assert_eq!(get(&router, "/healthz").await.0, StatusCode::OK);
        assert_eq!(
            get(&router, "/readyz").await.0,
            StatusCode::SERVICE_UNAVAILABLE,
            "not running: no heartbeat yet"
        );
        assert!(get(&router, "/metrics").await.1.contains("jobs_leader"));
    })
    .await;
}

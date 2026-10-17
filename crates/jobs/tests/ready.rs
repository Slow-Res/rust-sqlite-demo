#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::time::Duration;

use common::*;
use library_jobs::{Health, NotReady, ready};

#[tokio::test(flavor = "multi_thread")]
async fn readiness_follows_heartbeats_fencing_and_the_database() {
    with_db(|pool| async move {
        let stale_after = Duration::from_millis(500);
        let health = Health::default();
        assert_eq!(
            ready(&health, &pool, stale_after).await,
            Err(NotReady::NoHeartbeatYet)
        );
        health.beat_ok();
        assert_eq!(ready(&health, &pool, stale_after).await, Ok(()));
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert!(matches!(
            ready(&health, &pool, stale_after).await,
            Err(NotReady::StaleHeartbeat(_))
        ));
        health.beat_ok();
        health.set_fenced();
        let fenced = ready(&health, &pool, stale_after).await.unwrap_err();
        assert_eq!(fenced, NotReady::Fenced);
        assert!(fenced.to_string().contains("self-fenced"));

        // A pool that can never connect: port 1 refuses at once.
        let dead = sqlx::postgres::PgPoolOptions::new()
            .acquire_timeout(Duration::from_millis(500))
            .connect_lazy("postgres://unused@127.0.0.1:1/unused") // local only
            .unwrap();
        let fresh = Health::default();
        fresh.beat_ok();
        assert_eq!(
            ready(&fresh, &dead, Duration::from_secs(5)).await,
            Err(NotReady::DatabaseUnreachable)
        );
    })
    .await;
}

//! The whole worker loop against a real SQLite file and Postgres (spec §6.6–6.8).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{TestPg, TestSqlite, insert_book};
use sync_worker::config::{Config, LogFormat};
use sync_worker::metrics::Metrics;
use sync_worker::run::{self, Settings};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const NODE: &str = "node1";
const APP_NAME: &str = "sync-worker under test";

fn fast() -> Settings {
    Settings {
        batch_size: 500,
        poll_min: Duration::from_millis(10),
        poll_max: Duration::from_millis(50),
        retry_min: Duration::from_millis(50),
        retry_max: Duration::from_millis(200),
        lock_retry: Duration::from_millis(100),
        wait_step: Duration::from_millis(20),
    }
}

fn config(database_url: &str, pg: &TestPg) -> Config {
    Config {
        node_id: NODE.into(),
        database_url: database_url.into(),
        postgres: pg.options.clone().application_name(APP_NAME),
        batch_size: 500,
        metrics_addr: "127.0.0.1:0".parse().unwrap(),
        log_format: LogFormat::Pretty,
    }
}

fn start(
    config: Config,
) -> (
    JoinHandle<anyhow::Result<()>>,
    Arc<Metrics>,
    CancellationToken,
) {
    let metrics = Arc::new(Metrics::new());
    let token = CancellationToken::new();
    let task = tokio::spawn({
        let (metrics, token) = (metrics.clone(), token.clone());
        async move { run::run(&config, &fast(), &metrics, &token).await }
    });
    (task, metrics, token)
}

/// A COUNT on a table the worker has not created yet is 0 (Postgres error
/// 42P01, undefined_table); any other error fails the test.
fn zero_if_no_table(result: Result<i64, sqlx::Error>) -> i64 {
    match result {
        Ok(count) => count,
        Err(sqlx::Error::Database(err)) if err.code().as_deref() == Some("42P01") => 0,
        Err(err) => panic!("querying Postgres: {err}"),
    }
}

async fn stop(task: JoinHandle<anyhow::Result<()>>, token: CancellationToken) {
    token.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("run stops soon after shutdown")
        .unwrap()
        .unwrap();
}

/// Wait up to 10 s for a book with `title` to reach Postgres.
async fn wait_for_title(pg: &TestPg, title: &str) {
    let mut conn = pg.connect().await;
    for _ in 0..200 {
        let found = zero_if_no_table(
            sqlx::query_scalar("SELECT COUNT(*) FROM books WHERE node_id = $1 AND title = $2")
                .bind(NODE)
                .bind(title)
                .fetch_one(&mut conn)
                .await,
        );
        if found == 1 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("`{title}` never reached Postgres");
}

#[tokio::test]
async fn ships_changes_and_stops_cleanly_on_shutdown() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let (task, metrics, token) = start(config(&db.url, &pg));
    // Let the first cycle copy the empty node, so "hello" must ship as a batch.
    while metrics.last_success_seconds.get() == 0.0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    insert_book(&db.pool, "hello").await;
    wait_for_title(&pg, "hello").await;
    stop(task, token).await;
    // One batch of one row, and nothing left waiting.
    assert_eq!(metrics.batches.get(), 1);
    assert_eq!(metrics.rows_shipped.get(), 1);
    assert_eq!(metrics.epoch.get(), 1);
    assert_eq!(metrics.pending_changes.get(), 0);
    assert_eq!(metrics.errors.get(), 0);
    pg.cleanup().await;
}

#[tokio::test]
async fn reconnects_after_losing_the_postgres_connection() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let (task, metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "before").await;
    wait_for_title(&pg, "before").await;
    let mut conn = pg.connect().await;
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE application_name = $1 AND datname = current_database()",
    )
    .bind(APP_NAME)
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(killed, vec![true], "exactly one worker connection");
    insert_book(&db.pool, "after").await;
    wait_for_title(&pg, "after").await;
    assert!(metrics.errors.get() >= 1);
    drop(conn);
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn waits_for_the_database_file_to_appear() {
    let pg = TestPg::new().await;
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("library.db").display());
    let (task, _metrics, token) = start(config(&url, &pg));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!task.is_finished());
    assert!(
        !dir.path().join("library.db").exists(),
        "the worker never creates the database"
    );
    // Now library-web creates and migrates it.
    let pool = sqlite_setup::open_pool(&url, 2, true).await.unwrap();
    sync_worker::SQLITE_MIGRATIONS.run(&pool).await.unwrap();
    insert_book(&pool, "late").await;
    wait_for_title(&pg, "late").await;
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn waits_until_web_has_applied_every_migration() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    // Pretend the newest migration has not been applied yet.
    sqlx::query("CREATE TABLE saved_migration AS SELECT * FROM _sqlx_migrations WHERE version = (SELECT MAX(version) FROM _sqlx_migrations)")
        .execute(&db.pool).await.unwrap();
    sqlx::query(
        "DELETE FROM _sqlx_migrations WHERE version IN (SELECT version FROM saved_migration)",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    let (task, _metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "waiting").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut conn = pg.connect().await;
    let shipped = zero_if_no_table(
        sqlx::query_scalar("SELECT COUNT(*) FROM books")
            .fetch_one(&mut conn)
            .await,
    );
    assert_eq!(shipped, 0, "nothing ships before the schema is complete");
    // library-web finishes migrating.
    sqlx::query("INSERT INTO _sqlx_migrations SELECT * FROM saved_migration")
        .execute(&db.pool)
        .await
        .unwrap();
    wait_for_title(&pg, "waiting").await;
    drop(conn);
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_lock_held_elsewhere_blocks_shipping_until_it_is_released() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut holder = pg.connect().await;
    sqlx::query("SELECT pg_advisory_lock(hashtext('sync:' || $1))")
        .bind(NODE)
        .execute(&mut holder)
        .await
        .unwrap();
    let (task, metrics, token) = start(config(&db.url, &pg));
    insert_book(&db.pool, "locked").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let mut conn = pg.connect().await;
    let shipped = zero_if_no_table(
        sqlx::query_scalar("SELECT COUNT(*) FROM books")
            .fetch_one(&mut conn)
            .await,
    );
    assert_eq!(shipped, 0, "nothing ships while another holds the lock");
    assert!(metrics.errors.get() >= 1);
    assert!(
        metrics.pending_changes.get() >= 1,
        "the backlog is visible while locked out"
    );
    drop(holder);
    wait_for_title(&pg, "locked").await;
    drop(conn);
    stop(task, token).await;
    pg.cleanup().await;
}

#[tokio::test]
async fn backlog_gauges_stay_current_while_postgres_is_unreachable() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut cfg = config(&db.url, &pg);
    cfg.postgres = cfg.postgres.port(1);
    let (task, metrics, token) = start(cfg);
    insert_book(&db.pool, "stuck").await;
    for _ in 0..100 {
        if metrics.pending_changes.get() >= 1 && metrics.lag_seconds.get() > 0.0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(metrics.pending_changes.get() >= 1);
    assert!(metrics.lag_seconds.get() > 0.0);
    assert!(metrics.errors.get() >= 1);
    stop(task, token).await;
    pg.cleanup().await;
}

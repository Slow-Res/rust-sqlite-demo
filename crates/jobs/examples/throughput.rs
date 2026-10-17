//! No-op jobs per second with one worker process (spec §21: ≥ 1,000/s on the
//! 2-core box). Usage: cargo run --release -p library-jobs --example throughput -- <DATABASE_URL> [jobs] [slots]
//! The rows are bulk-inserted before the timer starts, so enqueueing is not measured: it
//! times claim + run + finish only. At start it deletes only earlier `bench_nothing.v1`
//! rows (other rows stay) and the succeeded rows are left behind: use a scratch database.

use std::time::{Duration, Instant};

use library_jobs::{Job, JobContext, JobError, QueueConfig, Registry, Worker, WorkerConfig};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

/// Longest the drain loop waits before it reports a stuck run.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Serialize, Deserialize)]
struct Nothing;

impl Job for Nothing {
    const KIND: &'static str = "bench_nothing.v1";
    const QUEUE: &'static str = "bench";
    async fn perform(self, _ctx: JobContext) -> Result<(), JobError> {
        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: throughput <DATABASE_URL> [jobs] [slots]"))?;
    let jobs: i64 = args.next().map_or(Ok(20_000), |s| s.parse())?;
    let slots: usize = args.next().map_or(Ok(64), |s| s.parse())?;
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(20)
        .connect(&url)
        .await?;
    library_jobs::migrate(&pool).await?;
    sqlx::query("DELETE FROM jobs WHERE kind = 'bench_nothing.v1'")
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO jobs (kind, queue, args, max_attempts, max_crashes, deadline_s, lease_s)
         SELECT 'bench_nothing.v1', 'bench', 'null', 3, 2, 60, 60 FROM generate_series(1, $1)",
    )
    .bind(jobs)
    .execute(&pool)
    .await?;
    let mut config = WorkerConfig::new(vec![QueueConfig {
        name: "bench".into(),
        slots,
        max_running: None,
    }]);
    config.poll = Duration::from_millis(50);
    config.maintenance.enabled = false;
    let worker = Worker::new(pool.clone(), Registry::new().register::<Nothing>(), config)?;
    let stop = CancellationToken::new();
    let handle = tokio::spawn(worker.run(stop.clone()));
    let start = Instant::now();
    let give_up = Instant::now() + DRAIN_TIMEOUT;
    loop {
        let left: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM jobs WHERE kind = 'bench_nothing.v1' AND state <> 'succeeded'",
        )
        .fetch_one(&pool)
        .await?;
        if left == 0 {
            break;
        }
        if Instant::now() > give_up {
            eprintln!("error: {left} of {jobs} jobs still not succeeded after {DRAIN_TIMEOUT:?}");
            std::process::exit(1);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let secs = start.elapsed().as_secs_f64();
    stop.cancel();
    handle.await??;
    println!(
        "{jobs} jobs in {secs:.2} s = {:.0} jobs/s ({slots} slots)",
        jobs as f64 / secs
    );
    Ok(())
}

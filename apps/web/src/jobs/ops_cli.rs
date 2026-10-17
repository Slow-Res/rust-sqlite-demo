//! `library-web jobs …`: read and repair the job tables (spec §17.4).

use std::fmt::Write as _;
use std::time::Duration;

use anyhow::bail;
use library_jobs::ops::{self, CancelOutcome, ListFilter, ProcessRow};
use sqlx::PgPool;

use crate::cli::JobsCommand;
use crate::config::Config;
use crate::jobs_config::JobsConfig;

/// A process this long without a heartbeat is shown as STALE (3 × the 20 s period).
const STALE_AFTER_S: f64 = 60.0;

pub async fn run(config: Config, command: JobsCommand) -> anyhow::Result<()> {
    let pool = super::connect(&config.database_url, 2).await?;
    let out = execute(&pool, &config.jobs, command).await;
    pool.close().await;
    print!("{}", out?);
    Ok(())
}

/// Run one command and return what to print.
pub async fn execute(
    pool: &PgPool,
    jobs: &JobsConfig,
    command: JobsCommand,
) -> anyhow::Result<String> {
    let mut out = String::new();
    match command {
        JobsCommand::Status => {
            let s = ops::status(pool).await?;
            writeln!(
                out,
                "{:<16} {:>9} {:>9} {:>7} {:>10} {:>9}  paused",
                "queue", "available", "scheduled", "running", "failed 24h", "oldest s"
            )?;
            for q in &s.queues {
                writeln!(
                    out,
                    "{:<16} {:>9} {:>9} {:>7} {:>10} {:>9.1}  {}",
                    q.queue,
                    q.available,
                    q.scheduled,
                    q.running,
                    q.failed_24h,
                    q.oldest_available_s,
                    if q.paused { "yes" } else { "" }
                )?;
            }
            if s.queues.is_empty() {
                writeln!(out, "(no waiting, running or recently failed jobs)")?;
            }
            writeln!(out)?;
            write_processes(&mut out, &s.processes)?;
            if !s.orphan_kinds.is_empty() {
                writeln!(
                    out,
                    "\nWARNING: no live process can run: {}",
                    s.orphan_kinds.join(", ")
                )?;
            }
        }
        JobsCommand::List {
            state,
            queue,
            kind,
            limit,
        } => {
            let rows = ops::list(
                pool,
                &ListFilter {
                    state,
                    queue,
                    kind,
                    limit,
                },
            )
            .await?;
            writeln!(
                out,
                "{:>10}  {:<16} {:<10} {:<9} {:>7} {:>7}  {:<20}  last error",
                "id", "kind", "queue", "state", "attempt", "crashes", "run at"
            )?;
            for j in rows {
                writeln!(
                    out,
                    "{:>10}  {:<16} {:<10} {:<9} {:>3}/{:<3} {:>7}  {:<20}  {}",
                    j.id,
                    j.kind,
                    j.queue,
                    j.state,
                    j.attempt,
                    j.max_attempts,
                    j.crashes,
                    j.run_at.format("%Y-%m-%d %H:%M:%S"),
                    j.last_error
                        .as_deref()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect::<String>()
                )?;
            }
        }
        JobsCommand::Show { id } => {
            let Some(j) = ops::show(pool, id).await? else {
                bail!("job {id} not found")
            };
            let s = &j.summary;
            writeln!(
                out,
                "id {}  kind {}  queue {}  state {}",
                s.id, s.kind, s.queue, s.state
            )?;
            writeln!(
                out,
                "attempt {}/{}  crashes {}  cancel requested {}",
                s.attempt, s.max_attempts, s.crashes, j.cancel_requested
            )?;
            writeln!(
                out,
                "created {}  run at {}  finished {:?}",
                s.created_at, s.run_at, s.finished_at
            )?;
            writeln!(
                out,
                "locked by {:?} until {:?}",
                j.locked_by, j.locked_until
            )?;
            writeln!(
                out,
                "unique key {:?}  concurrency key {:?}",
                j.unique_key, j.concurrency_key
            )?;
            writeln!(out, "args {}", serde_json::to_string_pretty(&j.args)?)?;
            writeln!(
                out,
                "progress {}",
                j.progress.map_or_else(|| "-".to_owned(), |p| p.to_string())
            )?;
            writeln!(out, "errors {}", serde_json::to_string_pretty(&j.errors)?)?;
        }
        JobsCommand::Retry { id: Some(id), .. } => {
            let done = ops::retry(pool, id).await?;
            writeln!(
                out,
                "{}",
                if done {
                    format!("job {id} is available again")
                } else {
                    format!(
                        "job {id} was not retried: it is not failed or cancelled, or another waiting or running job holds its unique key"
                    )
                }
            )?;
        }
        JobsCommand::Retry {
            id: None,
            kind,
            since,
            ..
        } => {
            let since = since.as_deref().map(parse_age).transpose()?;
            let n = ops::retry_failed(pool, kind.as_deref(), since).await?;
            writeln!(out, "{n} failed jobs are available again")?;
        }
        JobsCommand::Discard { id } => {
            let done = ops::discard(pool, id).await?;
            writeln!(
                out,
                "{}",
                if done {
                    format!("job {id} cancelled")
                } else {
                    format!("job {id} is not waiting or failed")
                }
            )?;
        }
        JobsCommand::Cancel { id } => {
            let msg = match ops::cancel(pool, id).await? {
                CancelOutcome::Cancelled => "cancelled",
                CancelOutcome::Requested => {
                    "running: cancellation requested, its handler stops within one heartbeat"
                }
                CancelOutcome::AlreadyFinished => "already finished",
                CancelOutcome::NotFound => "not found",
            };
            writeln!(out, "job {id}: {msg}")?;
        }
        JobsCommand::Pause { queue, reason } => {
            let by = std::env::var("USER").unwrap_or_else(|_| "cli".to_owned());
            ops::pause(pool, &queue, &by, reason.as_deref()).await?;
            writeln!(
                out,
                "queue {queue} paused; running jobs finish, nothing new is claimed"
            )?;
        }
        JobsCommand::Resume { queue } => {
            let done = ops::resume(pool, &queue).await?;
            writeln!(
                out,
                "{}",
                if done {
                    format!("queue {queue} resumed")
                } else {
                    format!("queue {queue} was not paused")
                }
            )?;
        }
        JobsCommand::Processes => write_processes(&mut out, &ops::processes(pool).await?)?,
        JobsCommand::Prune { .. } => {
            let keep_s = Duration::from_secs(jobs.keep_succeeded_h * 3600);
            let keep_f = Duration::from_secs(jobs.keep_failed_h * 3600);
            let (succeeded, failed) = ops::prune_preview(pool, keep_s, keep_f).await?;
            writeln!(
                out,
                "retention would delete {succeeded} succeeded and {failed} failed/cancelled jobs (the leader does it every minute)"
            )?;
        }
    }
    Ok(out)
}

fn write_processes(out: &mut String, processes: &[ProcessRow]) -> anyhow::Result<()> {
    writeln!(
        out,
        "{:>6}  {:<40} {:<9} {:>8}  {:>9}  state",
        "id", "name", "kind", "pid", "beat ago"
    )?;
    for p in processes {
        let state = if p.heartbeat_age_s > STALE_AFTER_S {
            "STALE"
        } else if p.stopping {
            "stopping"
        } else {
            "live"
        };
        writeln!(
            out,
            "{:>6}  {:<40} {:<9} {:>8}  {:>8.1}s  {}",
            p.id, p.name, p.kind, p.pid, p.heartbeat_age_s, state
        )?;
    }
    if processes.is_empty() {
        writeln!(out, "(no worker processes)")?;
    }
    Ok(())
}

/// `45s`, `30m`, `2h`, `7d`.
pub fn parse_age(raw: &str) -> anyhow::Result<Duration> {
    let bad = || anyhow::anyhow!("`{raw}` is not an age like 30m, 2h or 7d");
    let raw = raw.trim();
    // The unit is the last character, which may be multi-byte in bad input.
    let (split, unit) = raw.char_indices().next_back().ok_or_else(bad)?;
    let n: u64 = raw[..split].parse().map_err(|_| bad())?;
    let per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3600,
        'd' => 86_400,
        _ => return Err(bad()),
    };
    let secs = n.checked_mul(per_unit).ok_or_else(bad)?;
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ages() {
        assert_eq!(parse_age("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_age("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_age("7d").unwrap(), Duration::from_secs(604_800));
        assert_eq!(
            parse_age("18446744073709551615s").unwrap(),
            Duration::from_secs(u64::MAX)
        );
        for bad in [
            "",
            "h",
            "2",
            "2w",
            "-1h",
            "1.5h",
            "5é",
            "é",
            "日",
            "99999999999999999d",
            "18446744073709551615s1",
        ] {
            assert!(parse_age(bad).is_err(), "{bad}");
        }
    }
}

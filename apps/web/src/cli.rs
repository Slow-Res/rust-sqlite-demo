//! Command-line interface (clap). Adding a feature subcommand is two local
//! edits: a variant in [`Command`] and a match arm in `main.rs`.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "library-web", version, about = "Library management backend")]
pub struct Cli {
    /// Defaults to `serve` when omitted.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the HTTP server.
    Serve,
    /// Create a librarian account (prompts for the password twice).
    CreateUser {
        username: String,
        /// Read the password from the first line of stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Insert sample books, members and loans into an empty database.
    Seed,
    /// Check the database for corruption and broken references and print row
    /// counts. Never creates or migrates it (used on restored copies).
    CheckDb,
    /// Run background jobs: claim loops, heartbeats, maintenance, and
    /// /metrics /healthz /readyz on JOBS_METRICS_ADDR (Postgres, JOBS_ENABLED=true).
    #[cfg(feature = "postgres")]
    Worker,
    /// Inspect and repair background jobs.
    #[cfg(feature = "postgres")]
    Jobs {
        #[command(subcommand)]
        command: JobsCommand,
    },
}

/// `library-web jobs …` (spec §17.4).
#[cfg(feature = "postgres")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum JobsCommand {
    /// Per queue: available, scheduled, running, failed in 24 h, oldest wait; processes; orphan kinds.
    Status,
    /// Jobs, newest first.
    List {
        #[arg(long, value_parser = ["available", "running", "succeeded", "failed", "cancelled"])]
        state: Option<String>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// One job with its arguments, errors and progress.
    Show {
        id: i64,
    },
    /// Failed or cancelled → available, attempts reset: one job, or `--failed` in bulk.
    #[command(group(clap::ArgGroup::new("target").required(true).args(["id", "failed"])))]
    Retry {
        id: Option<i64>,
        #[arg(long, conflicts_with = "id")]
        failed: bool,
        #[arg(long, requires = "failed")]
        kind: Option<String>,
        /// Only jobs that failed within this age: 30m, 2h, 7d.
        #[arg(long, requires = "failed")]
        since: Option<String>,
    },
    /// A waiting (or failed) job → cancelled.
    Discard {
        id: i64,
    },
    /// Cancel a waiting job now, or ask a running one to stop.
    Cancel {
        id: i64,
    },
    /// Stop claiming from a queue (running jobs finish).
    Pause {
        queue: String,
        #[arg(long)]
        reason: Option<String>,
    },
    Resume {
        queue: String,
    },
    /// The process registry, with heartbeat ages.
    Processes,
    /// What retention would delete now (the leader deletes; this only counts).
    Prune {
        #[arg(long, required = true)]
        dry_run: bool,
    },
}

impl Cli {
    /// The chosen subcommand, or `serve` if none was given.
    pub fn command(self) -> Command {
        self.command.unwrap_or(Command::Serve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_means_serve() {
        let cli = Cli::parse_from(["library-web"]);
        assert_eq!(cli.command(), Command::Serve);
    }

    #[test]
    fn create_user_parses_username_and_flag() {
        let cli = Cli::parse_from(["library-web", "create-user", "admin", "--password-stdin"]);
        assert_eq!(
            cli.command(),
            Command::CreateUser {
                username: "admin".into(),
                password_stdin: true
            }
        );
    }

    #[test]
    fn seed_subcommand() {
        let cli = Cli::parse_from(["library-web", "seed"]);
        assert_eq!(cli.command(), Command::Seed);
    }

    #[test]
    fn check_db_subcommand() {
        assert_eq!(
            Cli::parse_from(["library-web", "check-db"]).command(),
            Command::CheckDb
        );
    }

    #[cfg(feature = "postgres")]
    #[test]
    fn worker_and_jobs_subcommands() {
        assert_eq!(
            Cli::parse_from(["library-web", "worker"]).command(),
            Command::Worker
        );
        assert_eq!(
            Cli::parse_from([
                "library-web",
                "jobs",
                "retry",
                "--failed",
                "--kind",
                "webhook.v1",
                "--since",
                "2h"
            ])
            .command(),
            Command::Jobs {
                command: JobsCommand::Retry {
                    id: None,
                    failed: true,
                    kind: Some("webhook.v1".into()),
                    since: Some("2h".into())
                }
            }
        );
        assert!(
            Cli::try_parse_from(["library-web", "jobs", "retry"]).is_err(),
            "an id or --failed is required"
        );
        assert!(Cli::try_parse_from(["library-web", "jobs", "list", "--state", "bogus"]).is_err());
        assert!(
            Cli::try_parse_from(["library-web", "jobs", "prune"]).is_err(),
            "only --dry-run exists"
        );
    }

    #[test]
    fn explicit_serve() {
        let cli = Cli::parse_from(["library-web", "serve"]);
        assert_eq!(cli.command(), Command::Serve);
    }
}

//! Command-line interface.

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "sync-worker",
    version,
    about = "Copies this node's SQLite changes to the central Postgres"
)]
pub struct Cli {
    /// Defaults to `run` when omitted.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the worker.
    Run,
    /// Print where this node's copy in Postgres stands and what is waiting.
    Status,
}

impl Cli {
    pub fn command(self) -> Command {
        self.command.unwrap_or(Command::Run)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_means_run() {
        assert_eq!(Cli::parse_from(["sync-worker"]).command(), Command::Run);
    }

    #[test]
    fn status_subcommand() {
        assert_eq!(
            Cli::parse_from(["sync-worker", "status"]).command(),
            Command::Status
        );
    }
}

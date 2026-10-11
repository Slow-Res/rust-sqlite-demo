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

    #[test]
    fn explicit_serve() {
        let cli = Cli::parse_from(["library-web", "serve"]);
        assert_eq!(cli.command(), Command::Serve);
    }
}

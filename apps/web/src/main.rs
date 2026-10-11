//! Thin entry point: load `.env`, parse the CLI, set up logging, dispatch.

use clap::Parser;
use library_web::cli::{Cli, Command};
use library_web::config::Config;
use library_web::{commands, server, telemetry};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env file is normal (production uses real env vars).
    dotenvy::dotenv().ok();

    let command = Cli::parse().command();
    let config = Config::from_env()?;
    telemetry::init(config.log_format);

    match command {
        Command::Serve => server::serve(config).await,
        Command::CreateUser {
            username,
            password_stdin,
        } => commands::create_user(config, &username, password_stdin).await,
        Command::Seed => commands::seed(config).await,
    }
}

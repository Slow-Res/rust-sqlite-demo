//! One-shot CLI commands that run instead of the server.

use std::io::BufRead;
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::auth::AuthService;
use crate::clock::{Clock, SystemClock};
use crate::config::Config;
use crate::db;
use crate::error::AppError;
use crate::seed::{self, SeedReport};
use crate::state::auth_repository;
use sqlite_setup::CheckReport;
use sqlx::SqlitePool;

/// `create-user`: add a librarian account to the database.
///
/// Errors (short password, mismatched confirmation, duplicate username) are
/// returned to `main`, which prints them and exits non-zero.
pub async fn create_user(
    config: Config,
    username: &str,
    password_stdin: bool,
) -> anyhow::Result<()> {
    // Read the password first so a typo fails before touching the database.
    // (Blocking on the terminal is fine here: nothing else is running yet.)
    let password = if password_stdin {
        read_password_from_stdin()?
    } else {
        prompt_password()?
    };

    let database = db::connect(&config).await?;
    db::migrate(&database).await?;
    let auth = AuthService::new(
        auth_repository(&database),
        Arc::new(SystemClock),
        chrono::Duration::hours(config.session_ttl_hours),
    );

    let result = auth.create_user(username, &password).await;
    database.close().await;
    // `AppError`'s message is user-facing for validation and conflict errors.
    let user = result.map_err(|err| describe(&err))?;
    println!("Created user `{}` (id {})", user.username, user.id);
    Ok(())
}

/// `seed`: fill an empty database with sample books, members and loans.
pub async fn seed(config: Config) -> anyhow::Result<()> {
    let database = db::connect(&config).await?;
    // Checked before migrating, so a refused command leaves a Postgres database untouched.
    let pool = database.require_sqlite("seed")?;
    db::migrate(&database).await?;
    let result = seed::seed(pool, SystemClock.now()).await;
    database.close().await;
    match result? {
        SeedReport::Seeded {
            books,
            members,
            loans,
        } => println!("Seeded {books} books, {members} members and {loans} loans"),
        SeedReport::Skipped => println!("database already has data; skipping"),
    }
    Ok(())
}

/// Tables whose row counts `check-db` prints, so a restored copy can be
/// compared with the live database.
const COUNTED_TABLES: [(&str, &str); 4] = [
    ("books", "SELECT COUNT(*) FROM books"),
    ("members", "SELECT COUNT(*) FROM members"),
    ("loans", "SELECT COUNT(*) FROM loans"),
    ("users", "SELECT COUNT(*) FROM users"),
];

/// What `check-db` found.
#[derive(Debug)]
pub struct DbInspection {
    pub report: CheckReport,
    pub counts: Vec<(&'static str, i64)>,
}

pub async fn inspect(pool: &SqlitePool) -> anyhow::Result<DbInspection> {
    let report = sqlite_setup::check(pool).await?;
    let mut counts = Vec::new();
    for (table, sql) in COUNTED_TABLES {
        let count: i64 = sqlx::query_scalar(sql)
            .fetch_one(pool)
            .await
            .with_context(|| format!("counting {table}"))?;
        counts.push((table, count));
    }
    Ok(DbInspection { report, counts })
}

pub fn render(database_url: &str, inspection: &DbInspection) -> String {
    let mut out = format!("database: {database_url}\n");
    let section = |out: &mut String, name: &str, errors: &[String]| {
        if errors.is_empty() {
            out.push_str(&format!("{name}: ok\n"));
        }
        for error in errors {
            out.push_str(&format!("{name}: {error}\n"));
        }
    };
    section(&mut out, "integrity", &inspection.report.integrity_errors);
    section(
        &mut out,
        "foreign keys",
        &inspection.report.foreign_key_errors,
    );
    for (table, count) in &inspection.counts {
        out.push_str(&format!("{table}: {count}\n"));
    }
    out
}

/// `check-db`: look for corruption and broken references, and print row
/// counts (spec §5 S15). It never creates or migrates the database, because it
/// is used on restored copies, which must be inspected exactly as they are.
pub async fn check_db(config: Config) -> anyhow::Result<()> {
    if db::is_postgres_url(&config.database_url) {
        bail!("`check-db` supports only SQLite, not Postgres");
    }
    let pool = sqlite_setup::open_pool(&config.database_url, 1, false).await?;
    let inspection = inspect(&pool).await;
    pool.close().await;
    let inspection = inspection?;
    print!("{}", render(&config.database_url, &inspection));
    if !inspection.report.is_ok() {
        bail!("database check failed");
    }
    Ok(())
}

/// Turn a service error into one `main` can print. Internal errors keep their
/// cause chain (`{:#}` joins it with `: `) because `AppError::Internal`'s own
/// message is only "internal error".
fn describe(err: &AppError) -> anyhow::Error {
    match err {
        AppError::Validation(fields) => anyhow::anyhow!(
            "{}",
            fields
                .iter()
                .map(|(field, message)| format!("{field} {message}"))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        AppError::Internal(source) => anyhow::anyhow!("internal error: {source:#}"),
        other => anyhow::anyhow!("{other}"),
    }
}

fn prompt_password() -> anyhow::Result<String> {
    let password = rpassword::prompt_password("Password: ").context("reading password")?;
    let confirm = rpassword::prompt_password("Repeat password: ").context("reading password")?;
    if password != confirm {
        bail!("passwords do not match");
    }
    Ok(password)
}

fn read_password_from_stdin() -> anyhow::Result<String> {
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading password from stdin")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_owned())
}

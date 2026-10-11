//! sync-worker: copies one node's SQLite changes to the central Postgres
//! (spec §6). `main.rs` is a thin shell; everything testable lives here.

pub mod changes;
pub mod config;
pub mod tables;

/// The SQLite migrations, embedded so the worker can check that `library-web`
/// has applied them all before it ships anything (spec §6.7). The worker never
/// runs them: `library-web` is the only program that migrates SQLite.
// apps/sync-worker/build.rs rebuilds this crate when either folder changes.
pub static SQLITE_MIGRATIONS: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations/sqlite");

/// The Postgres migrations. The worker runs these when it connects.
pub static POSTGRES_MIGRATIONS: sqlx::migrate::Migrator =
    sqlx::migrate!("../../migrations/postgres");

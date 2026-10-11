//! Waiting for `library-web` to migrate SQLite before shipping (spec §6.7).
//! During a rolling deploy the new worker can start before the new web app
//! has migrated; reading a column that does not exist yet would fail.

use std::collections::BTreeSet;

use anyhow::Context;
use sqlx::SqlitePool;

/// True once every migration this worker was built with is recorded as
/// applied in `_sqlx_migrations`.
pub async fn sqlite_schema_ready(pool: &SqlitePool) -> anyhow::Result<bool> {
    let has_table: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = '_sqlx_migrations')",
    )
    .fetch_one(pool)
    .await
    .context("looking for _sqlx_migrations")?;
    if !has_table {
        return Ok(false);
    }
    let applied: BTreeSet<i64> =
        sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success = 1")
            .fetch_all(pool)
            .await
            .context("reading _sqlx_migrations")?
            .into_iter()
            .collect();
    Ok(crate::SQLITE_MIGRATIONS
        .iter()
        .filter(|m| m.migration_type.is_up_migration())
        .all(|m| applied.contains(&m.version)))
}

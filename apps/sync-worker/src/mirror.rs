//! The Postgres side of the worker (spec §6.2, §6.5, §6.6).
//!
//! A [`Mirror`] owns exactly one Postgres connection. That connection holds
//! the node's advisory lock, and every write goes through it. If the
//! connection drops, Postgres releases the lock *and* our writes fail, so a
//! worker can never write without holding the lock.

use std::time::Duration;

use anyhow::Context;
use sqlx::postgres::{PgConnectOptions, PgRow};
use sqlx::{AssertSqlSafe, Connection, PgConnection, Postgres, Row as _, Transaction};

use crate::changes::{Batch, Checkpoint, Snapshot};
use crate::tables::{self, Kind, Row, Table, Value};

/// Rows per INSERT during a full copy.
const COPY_CHUNK: usize = 1000;

/// Without these, a half-open connection (a dead network path) would hang a
/// cycle until the OS gives up, about 15 minutes.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const STATEMENT_TIMEOUT: &str = "60s";

/// `options` plus the server-side statement timeout every worker session uses.
pub fn session_options(options: &PgConnectOptions) -> PgConnectOptions {
    options
        .clone()
        .options([("statement_timeout", STATEMENT_TIMEOUT)])
}

/// Connect with the session options, giving up after [`CONNECT_TIMEOUT`].
pub async fn connect_with_timeouts(options: &PgConnectOptions) -> anyhow::Result<PgConnection> {
    tokio::time::timeout(
        CONNECT_TIMEOUT,
        PgConnection::connect_with(&session_options(options)),
    )
    .await
    .context("timed out connecting to Postgres")?
    .context("connecting to Postgres")
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// Another worker holds this node's lock (spec §6.6).
    #[error("another sync-worker holds the lock for node `{0}`; is NODE_ID used twice?")]
    Locked(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// What a full copy did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceReport {
    /// The new epoch.
    pub epoch: i32,
    /// Rows moved to `sync_archive`.
    pub archived: u64,
    /// Rows copied from the snapshot.
    pub copied: u64,
}

pub struct Mirror {
    conn: PgConnection,
    node_id: String,
}

impl Mirror {
    /// Connect, run the Postgres migrations, and take the node's lock.
    pub async fn connect(options: &PgConnectOptions, node_id: &str) -> Result<Self, ConnectError> {
        let mut conn = connect_with_timeouts(options).await?;
        // run_direct, not run: `run` goes through `Acquire`, which makes the
        // worker's future fail the `Send` check when it is `tokio::spawn`ed.
        crate::POSTGRES_MIGRATIONS
            .run_direct(None, &mut conn, false)
            .await
            .context("running the Postgres migrations")?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(format!("sync:{node_id}"))
            .fetch_one(&mut conn)
            .await
            .context("taking the node lock")?;
        if !locked {
            conn.close().await.ok();
            return Err(ConnectError::Locked(node_id.to_owned()));
        }
        Ok(Self {
            conn,
            node_id: node_id.to_owned(),
        })
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub async fn load_checkpoint(&mut self) -> anyhow::Result<Option<Checkpoint>> {
        load_checkpoint(&mut self.conn, &self.node_id).await
    }

    /// Apply one batch and move the checkpoint, in one transaction (spec §6.3).
    pub async fn apply_batch(&mut self, batch: &Batch, epoch: i32) -> anyhow::Result<()> {
        let mut tx = self
            .conn
            .begin()
            .await
            .context("starting a Postgres transaction")?;
        for changes in &batch.tables {
            upsert(&mut tx, &self.node_id, changes.table, &changes.upserts).await?;
            delete(&mut tx, &self.node_id, changes.table, &changes.deletes).await?;
        }
        let checkpoint = Checkpoint {
            epoch,
            last_seq: batch.last_seq,
            last_token: Some(batch.last_token.clone()),
        };
        save_checkpoint(&mut tx, &self.node_id, &checkpoint).await?;
        tx.commit().await.context("committing the batch")?;
        Ok(())
    }

    /// Replace this node's whole copy with `snapshot`, in one transaction
    /// (spec §6.5). Rows already there go to `sync_archive` under `old_epoch`;
    /// the checkpoint gets epoch `old_epoch + 1`.
    pub async fn replace_all(
        &mut self,
        snapshot: &Snapshot,
        old_epoch: i32,
    ) -> anyhow::Result<ReplaceReport> {
        let epoch = old_epoch + 1;
        let mut tx = self
            .conn
            .begin()
            .await
            .context("starting a Postgres transaction")?;
        let mut archived = 0;
        for table in tables::TABLES {
            let archive = format!(
                "INSERT INTO sync_archive (node_id, epoch, table_name, row_data) \
                 SELECT node_id, $2, '{name}', to_jsonb(t) FROM {name} t WHERE node_id = $1",
                name = table.name
            );
            archived += sqlx::query(AssertSqlSafe(archive))
                .bind(self.node_id.clone())
                .bind(old_epoch)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("archiving {} rows", table.name))?
                .rows_affected();
            sqlx::query(AssertSqlSafe(format!(
                "DELETE FROM {} WHERE node_id = $1",
                table.name
            )))
            .bind(self.node_id.clone())
            .execute(&mut *tx)
            .await
            .with_context(|| format!("clearing {} rows", table.name))?;
        }
        let mut copied = 0;
        for changes in &snapshot.tables {
            for chunk in changes.upserts.chunks(COPY_CHUNK) {
                upsert(&mut tx, &self.node_id, changes.table, chunk).await?;
                copied += chunk.len() as u64;
            }
        }
        let checkpoint = Checkpoint {
            epoch,
            last_seq: snapshot.seq,
            last_token: snapshot.token.clone(),
        };
        save_checkpoint(&mut tx, &self.node_id, &checkpoint).await?;
        tx.commit().await.context("committing the full copy")?;
        Ok(ReplaceReport {
            epoch,
            archived,
            copied,
        })
    }

    /// Every row Postgres holds for this node in `table`, ordered by id.
    pub async fn read_all(&mut self, table: &Table) -> anyhow::Result<Vec<Row>> {
        read_all(&mut self.conn, &self.node_id, table).await
    }

    /// Hang up cleanly, which releases the node's lock.
    pub async fn close(self) {
        self.conn.close().await.ok();
    }
}

pub async fn load_checkpoint(
    conn: &mut PgConnection,
    node_id: &str,
) -> anyhow::Result<Option<Checkpoint>> {
    let row: Option<(i32, i64, Option<Vec<u8>>)> = sqlx::query_as(
        "SELECT epoch, last_seq, last_token FROM sync_checkpoints WHERE node_id = $1",
    )
    .bind(node_id)
    .fetch_optional(conn)
    .await
    .context("reading the checkpoint")?;
    Ok(row.map(|(epoch, last_seq, last_token)| Checkpoint {
        epoch,
        last_seq,
        last_token,
    }))
}

async fn save_checkpoint(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    checkpoint: &Checkpoint,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO sync_checkpoints (node_id, epoch, last_seq, last_token, updated_at) \
         VALUES ($1, $2, $3, $4, now()) \
         ON CONFLICT (node_id) DO UPDATE SET epoch = EXCLUDED.epoch, last_seq = EXCLUDED.last_seq, \
         last_token = EXCLUDED.last_token, updated_at = now()",
    )
    .bind(node_id)
    .bind(checkpoint.epoch)
    .bind(checkpoint.last_seq)
    .bind(checkpoint.last_token.clone())
    .execute(&mut **tx)
    .await
    .context("saving the checkpoint")?;
    Ok(())
}

/// `INSERT ... SELECT $1, * FROM UNNEST(...) ON CONFLICT DO UPDATE`: one
/// statement and one round trip for any number of rows (spec §6.3).
pub fn upsert_sql(table: &Table) -> String {
    let arrays: Vec<String> = table
        .columns
        .iter()
        .enumerate()
        .map(|(i, c)| format!("${}::{}[]", i + 2, c.kind.pg_type()))
        .collect();
    let updates: Vec<String> = table
        .columns
        .iter()
        .skip(1)
        .map(|c| format!("{0} = EXCLUDED.{0}", c.name))
        .collect();
    format!(
        "INSERT INTO {name} (node_id, {columns}) SELECT $1::text, * FROM UNNEST({arrays}) \
         ON CONFLICT (node_id, id) DO UPDATE SET {updates}, synced_at = now()",
        name = table.name,
        columns = table.column_list(),
        arrays = arrays.join(", "),
        updates = updates.join(", "),
    )
}

async fn upsert(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    table: &Table,
    rows: &[Row],
) -> anyhow::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    // One array per column; NOT NULL columns are enforced by Postgres itself.
    let mut query = sqlx::query(AssertSqlSafe(upsert_sql(table))).bind(node_id.to_owned());
    // A fn, not a closure: a closure cannot tie its returned reference to its argument.
    fn cell<'a>(row: &'a Row, i: usize, table: &Table) -> anyhow::Result<&'a Value> {
        row.get(i).with_context(|| {
            format!(
                "{} row is missing column {}",
                table.name, table.columns[i].name
            )
        })
    }
    for (i, column) in table.columns.iter().enumerate() {
        query = match column.kind {
            Kind::Int => query.bind(
                rows.iter()
                    .map(|r| cell(r, i, table)?.as_int())
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
            Kind::Text => query.bind(
                rows.iter()
                    .map(|r| cell(r, i, table)?.as_text())
                    .collect::<anyhow::Result<Vec<_>>>()?,
            ),
        };
    }
    query
        .execute(&mut **tx)
        .await
        .with_context(|| format!("upserting {} {} rows", rows.len(), table.name))?;
    Ok(())
}

async fn delete(
    tx: &mut Transaction<'_, Postgres>,
    node_id: &str,
    table: &Table,
    ids: &[i64],
) -> anyhow::Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    sqlx::query(AssertSqlSafe(format!(
        "DELETE FROM {} WHERE node_id = $1 AND id = ANY($2)",
        table.name
    )))
    .bind(node_id.to_owned())
    .bind(ids.to_vec())
    .execute(&mut **tx)
    .await
    .with_context(|| format!("deleting {} {} rows", ids.len(), table.name))?;
    Ok(())
}

pub async fn read_all(
    conn: &mut PgConnection,
    node_id: &str,
    table: &Table,
) -> anyhow::Result<Vec<Row>> {
    let sql = format!(
        "SELECT {} FROM {} WHERE node_id = $1 ORDER BY id",
        table.column_list(),
        table.name
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(node_id.to_owned())
        .fetch_all(conn)
        .await
        .with_context(|| format!("reading {} from Postgres", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

fn decode(table: &Table, row: &PgRow) -> anyhow::Result<Row> {
    table
        .columns
        .iter()
        .enumerate()
        .map(|(i, column)| -> anyhow::Result<Value> {
            Ok(match column.kind {
                Kind::Int => row
                    .try_get::<Option<i64>, _>(i)?
                    .map_or(Value::Null, Value::Int),
                Kind::Text => row
                    .try_get::<Option<String>, _>(i)?
                    .map_or(Value::Null, Value::Text),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_sql_for_members() {
        assert_eq!(
            upsert_sql(tables::by_name("members").unwrap()),
            "INSERT INTO members (node_id, id, name, email, created_at, updated_at) \
             SELECT $1::text, * FROM UNNEST($2::bigint[], $3::text[], $4::text[], $5::bigint[], $6::bigint[]) \
             ON CONFLICT (node_id, id) DO UPDATE SET name = EXCLUDED.name, email = EXCLUDED.email, \
             created_at = EXCLUDED.created_at, updated_at = EXCLUDED.updated_at, synced_at = now()"
        );
    }
}

//! The SQLite side of the worker (spec §6.3–6.5): reading the change log,
//! the changed rows and full snapshots, and pruning shipped log rows.
//!
//! Each read runs in one SQLite read transaction, so everything it returns
//! comes from one moment in time, while `library-web` keeps writing (WAL lets
//! readers and the writer work side by side).

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, bail};
use sqlx::sqlite::SqliteRow;
use sqlx::{AssertSqlSafe, Row as _, Sqlite, SqliteExecutor, SqlitePool, Transaction};

use crate::tables::{self, Kind, Row, Table, Value};

/// Where Postgres's copy of a node stands: one row of `sync_checkpoints`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// Increases by one on every full copy.
    pub epoch: i32,
    /// Highest `change_log.seq` already applied to Postgres; 0 before any.
    pub last_seq: i64,
    /// `token` of that change_log row; `None` when `last_seq` is 0.
    pub last_token: Option<Vec<u8>>,
}

/// One change_log row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub seq: i64,
    pub table_name: String,
    pub row_id: i64,
    pub token: Vec<u8>,
}

/// What changed in one table: rows to insert or update, and ids to delete.
#[derive(Debug, Clone, PartialEq)]
pub struct TableChanges {
    pub table: &'static Table,
    pub upserts: Vec<Row>,
    pub deletes: Vec<i64>,
}

/// One batch, ready to ship.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    /// change_log rows read, before coalescing.
    pub changes: usize,
    /// `seq` and `token` of the last change_log row in the batch; they become
    /// the new checkpoint.
    pub last_seq: i64,
    pub last_token: Vec<u8>,
    /// Only tables with changes, in [`tables::TABLES`] order.
    pub tables: Vec<TableChanges>,
}

impl Batch {
    /// Rows Postgres will insert, update or delete.
    pub fn rows(&self) -> usize {
        self.tables
            .iter()
            .map(|t| t.upserts.len() + t.deletes.len())
            .sum()
    }
}

/// Every row of every synced table, and the change_log position it matches.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// `seq` of the newest change_log row, or 0 if the log is empty.
    pub seq: i64,
    /// Its token; `None` when `seq` is 0.
    pub token: Option<Vec<u8>>,
    /// One entry per synced table; `deletes` is always empty.
    pub tables: Vec<TableChanges>,
}

/// Result of [`read_batch`].
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// The checkpoint does not match this database: it was restored from an
    /// older backup, or Postgres was (spec §6.4). A full copy is needed.
    Invalid,
    /// Nothing new since the checkpoint.
    Empty,
    Batch(Batch),
}

/// Changes not yet in Postgres.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pending {
    pub count: i64,
    /// `created_at` (Unix ms) of the oldest one.
    pub oldest_ms: Option<i64>,
}

/// Read the next batch after `checkpoint`, or report that the checkpoint no
/// longer matches this database.
pub async fn read_batch(
    pool: &SqlitePool,
    checkpoint: &Checkpoint,
    limit: usize,
) -> anyhow::Result<Read> {
    let mut tx = pool
        .begin()
        .await
        .context("starting a SQLite read transaction")?;
    if !checkpoint_is_valid(&mut tx, checkpoint).await? {
        return Ok(Read::Invalid);
    }
    let changes = read_changes(&mut tx, checkpoint.last_seq, limit).await?;
    let Some(last) = changes.last() else {
        return Ok(Read::Empty);
    };
    let (last_seq, last_token) = (last.seq, last.token.clone());

    let mut per_table = Vec::new();
    for (table, ids) in coalesce(&changes)? {
        let upserts = read_rows(&mut tx, table, &ids).await?;
        let present: BTreeSet<i64> = upserts.iter().filter_map(tables::row_id).collect();
        let deletes = ids.difference(&present).copied().collect();
        per_table.push(TableChanges {
            table,
            upserts,
            deletes,
        });
    }
    tx.commit()
        .await
        .context("ending the SQLite read transaction")?;

    Ok(Read::Batch(Batch {
        changes: changes.len(),
        last_seq,
        last_token,
        tables: per_table,
    }))
}

/// Group change_log rows into the distinct row ids of each table, in
/// [`tables::TABLES`] order: 50 changes to one row become one id. An unknown
/// table name is an error, never skipped: it means this worker is older than
/// the schema and must be upgraded.
pub fn coalesce(changes: &[Change]) -> anyhow::Result<Vec<(&'static Table, BTreeSet<i64>)>> {
    let mut by_table: BTreeMap<&str, BTreeSet<i64>> = BTreeMap::new();
    for change in changes {
        if tables::by_name(&change.table_name).is_none() {
            bail!(
                "change_log seq {} names table `{}`, which this sync-worker does not know; upgrade the worker",
                change.seq,
                change.table_name
            );
        }
        by_table
            .entry(change.table_name.as_str())
            .or_default()
            .insert(change.row_id);
    }
    Ok(tables::TABLES
        .iter()
        .filter_map(|table| by_table.remove(table.name).map(|ids| (table, ids)))
        .collect())
}

/// The check from spec §6.4: valid if nothing was shipped yet, or if the row
/// the checkpoint points at is still here with the same token.
async fn checkpoint_is_valid(
    tx: &mut Transaction<'_, Sqlite>,
    checkpoint: &Checkpoint,
) -> anyhow::Result<bool> {
    if checkpoint.last_seq == 0 {
        return Ok(true);
    }
    let token: Option<Vec<u8>> = sqlx::query_scalar("SELECT token FROM change_log WHERE seq = ?")
        .bind(checkpoint.last_seq)
        .fetch_optional(&mut **tx)
        .await
        .context("looking up the checkpoint row")?;
    Ok(matches!((token, &checkpoint.last_token), (Some(here), Some(there)) if &here == there))
}

async fn read_changes(
    tx: &mut Transaction<'_, Sqlite>,
    after_seq: i64,
    limit: usize,
) -> anyhow::Result<Vec<Change>> {
    let limit = i64::try_from(limit).context("batch size too large")?;
    let rows = sqlx::query(
        "SELECT seq, table_name, row_id, token FROM change_log WHERE seq > ? ORDER BY seq LIMIT ?",
    )
    .bind(after_seq)
    .bind(limit)
    .fetch_all(&mut **tx)
    .await
    .context("reading change_log")?;
    let changes = rows
        .iter()
        .map(|r| -> Result<Change, sqlx::Error> {
            Ok(Change {
                seq: r.try_get(0)?,
                table_name: r.try_get(1)?,
                row_id: r.try_get(2)?,
                token: r.try_get(3)?,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(changes)
}

/// The current values of the rows with these ids that still exist.
async fn read_rows(
    tx: &mut Transaction<'_, Sqlite>,
    table: &Table,
    ids: &BTreeSet<i64>,
) -> anyhow::Result<Vec<Row>> {
    let ids_json = serde_json::to_string(&ids.iter().collect::<Vec<_>>())?;
    // Table and column names come from the fixed list in tables.rs, never from input.
    let sql = format!(
        "SELECT {} FROM {} WHERE id IN (SELECT value FROM json_each(?)) ORDER BY id",
        table.column_list(),
        table.name
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(ids_json)
        .fetch_all(&mut **tx)
        .await
        .with_context(|| format!("reading changed {} rows", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

/// Every row of `table`, ordered by id.
pub async fn read_all<'e>(
    executor: impl SqliteExecutor<'e>,
    table: &Table,
) -> anyhow::Result<Vec<Row>> {
    let sql = format!(
        "SELECT {} FROM {} ORDER BY id",
        table.column_list(),
        table.name
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .fetch_all(executor)
        .await
        .with_context(|| format!("reading all {} rows", table.name))?;
    rows.iter().map(|r| decode(table, r)).collect()
}

fn decode(table: &Table, row: &SqliteRow) -> anyhow::Result<Row> {
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

/// Every row of every synced table plus the newest change_log position, all
/// from one moment (spec §6.5).
pub async fn read_snapshot(pool: &SqlitePool) -> anyhow::Result<Snapshot> {
    let mut tx = pool
        .begin()
        .await
        .context("starting a SQLite read transaction")?;
    let newest: Option<(i64, Vec<u8>)> =
        sqlx::query_as("SELECT seq, token FROM change_log ORDER BY seq DESC LIMIT 1")
            .fetch_optional(&mut *tx)
            .await
            .context("reading the newest change_log row")?;
    let (seq, token) = match newest {
        Some((seq, token)) => (seq, Some(token)),
        None => (0, None),
    };
    let mut per_table = Vec::new();
    for table in tables::TABLES {
        let upserts = read_all(&mut *tx, table).await?;
        per_table.push(TableChanges {
            table,
            upserts,
            deletes: Vec::new(),
        });
    }
    tx.commit()
        .await
        .context("ending the SQLite read transaction")?;
    Ok(Snapshot {
        seq,
        token,
        tables: per_table,
    })
}

/// Delete change_log rows below `seq`. The row *at* `seq` stays: it is the
/// checkpoint row that the next cycle's check looks for (spec §6.4).
pub async fn prune(pool: &SqlitePool, below_seq: i64) -> anyhow::Result<u64> {
    let done = sqlx::query("DELETE FROM change_log WHERE seq < ?")
        .bind(below_seq)
        .execute(pool)
        .await
        .context("pruning change_log")?;
    Ok(done.rows_affected())
}

/// How many changes after `after_seq` are waiting, and how old the oldest is.
pub async fn pending(pool: &SqlitePool, after_seq: i64) -> anyhow::Result<Pending> {
    let (count, oldest_ms): (i64, Option<i64>) =
        sqlx::query_as("SELECT COUNT(*), MIN(created_at) FROM change_log WHERE seq > ?")
            .bind(after_seq)
            .fetch_one(pool)
            .await
            .context("counting pending changes")?;
    Ok(Pending { count, oldest_ms })
}

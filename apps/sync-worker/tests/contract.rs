//! The contract between the SQLite schema, the Postgres schema and the
//! worker's table list (spec §5 S11, §6.9). If someone changes a synced table
//! in one place only, this test names the table.
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestPg, TestSqlite};
use sqlx::{AssertSqlSafe, Row};
use sync_worker::tables::{self, Kind};

async fn primary_key(conn: &mut sqlx::PgConnection, table: &str) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT k.column_name::text FROM information_schema.table_constraints c \
         JOIN information_schema.key_column_usage k \
           ON k.constraint_name = c.constraint_name AND k.table_schema = c.table_schema \
          AND k.table_name = c.table_name \
         WHERE c.table_schema = 'public' AND c.table_name = $1 AND c.constraint_type = 'PRIMARY KEY' \
         ORDER BY k.ordinal_position",
    )
    .bind(table)
    .fetch_all(conn)
    .await
    .unwrap()
}

#[tokio::test]
async fn synced_tables_match_in_sqlite_postgres_and_the_table_list() {
    let sqlite = TestSqlite::new().await;
    let pg = TestPg::new().await;
    let mut conn = pg.connect().await;
    sync_worker::POSTGRES_MIGRATIONS
        .run(&mut conn)
        .await
        .unwrap();

    for table in tables::TABLES {
        // SQLite columns: (name, declared type, nullable). An INTEGER PRIMARY
        // KEY reports notnull = 0 but can never be NULL.
        let rows = sqlx::query(AssertSqlSafe(format!("PRAGMA table_info({})", table.name)))
            .fetch_all(&sqlite.pool)
            .await
            .unwrap();
        let in_sqlite: Vec<(String, String, bool)> = rows
            .iter()
            .map(|r| {
                let notnull: i64 = r.get("notnull");
                let pk: i64 = r.get("pk");
                (r.get("name"), r.get("type"), notnull == 0 && pk == 0)
            })
            .collect();
        let expected: Vec<(String, String, bool)> = table
            .columns
            .iter()
            .map(|c| {
                let ty = match c.kind {
                    Kind::Int => "INTEGER",
                    Kind::Text => "TEXT",
                };
                (c.name.to_owned(), ty.to_owned(), c.nullable)
            })
            .collect();
        assert_eq!(
            in_sqlite, expected,
            "SQLite table `{}` differs from tables.rs",
            table.name
        );

        let in_postgres: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT column_name::text, data_type::text, is_nullable::text FROM information_schema.columns \
             WHERE table_schema = 'public' AND table_name = $1 ORDER BY ordinal_position",
        )
        .bind(table.name)
        .fetch_all(&mut conn)
        .await
        .unwrap();
        let mut expected: Vec<(String, String, String)> =
            vec![("node_id".into(), "text".into(), "NO".into())];
        expected.extend(table.columns.iter().map(|c| {
            let nullable = if c.nullable { "YES" } else { "NO" };
            (
                c.name.to_owned(),
                c.kind.pg_type().to_owned(),
                nullable.to_owned(),
            )
        }));
        expected.push((
            "synced_at".into(),
            "timestamp with time zone".into(),
            "NO".into(),
        ));
        assert_eq!(
            in_postgres, expected,
            "Postgres table `{}` differs from tables.rs",
            table.name
        );

        // Task 7's upsert relies on `ON CONFLICT (node_id, id)`.
        assert_eq!(
            primary_key(&mut conn, table.name).await,
            ["node_id", "id"],
            "primary key of Postgres table `{}`",
            table.name
        );

        let triggers: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name = ? ORDER BY name",
        )
        .bind(table.name)
        .fetch_all(&sqlite.pool)
        .await
        .unwrap();
        let expected: Vec<String> = ["delete", "insert", "update"]
            .iter()
            .map(|op| format!("{}_logged_{op}", table.name))
            .collect();
        assert_eq!(
            triggers, expected,
            "change-log triggers on `{}`",
            table.name
        );
    }

    // Tables that are not synced must not fill the change log.
    let other_triggers: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'trigger' AND tbl_name NOT IN ('books', 'members', 'loans')",
    )
    .fetch_all(&sqlite.pool)
    .await
    .unwrap();
    assert!(other_triggers.is_empty(), "{other_triggers:?}");

    assert_eq!(
        primary_key(&mut conn, "sync_checkpoints").await,
        ["node_id"]
    );
    let last_token_nullable: String = sqlx::query_scalar(
        "SELECT is_nullable::text FROM information_schema.columns \
         WHERE table_schema = 'public' AND table_name = 'sync_checkpoints' AND column_name = 'last_token'",
    )
    .fetch_one(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        last_token_nullable, "YES",
        "sync_checkpoints.last_token must allow NULL"
    );

    drop(conn);
    pg.cleanup().await;
}

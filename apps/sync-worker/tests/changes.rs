//! The SQLite side of the worker (spec §6.3–6.5), on a real database file.
#![allow(clippy::unwrap_used)]

mod common;

use common::{
    TestSqlite, delete_book, insert_book, insert_books, insert_loan, insert_member, rename_book,
};
use sync_worker::changes::{self, Batch, Checkpoint, Read};
use sync_worker::tables::{self, Value};

const START: Checkpoint = Checkpoint {
    epoch: 1,
    last_seq: 0,
    last_token: None,
};

fn batch(read: Read) -> Batch {
    match read {
        Read::Batch(b) => b,
        other => panic!("expected a batch, got {other:?}"),
    }
}

fn after(b: &Batch) -> Checkpoint {
    Checkpoint {
        epoch: 1,
        last_seq: b.last_seq,
        last_token: Some(b.last_token.clone()),
    }
}

#[tokio::test]
async fn nothing_to_read_on_a_new_database() {
    let db = TestSqlite::new().await;
    assert_eq!(
        changes::read_batch(&db.pool, &START, 500).await.unwrap(),
        Read::Empty
    );
}

#[tokio::test]
async fn an_insert_is_read_with_its_current_values() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "Dune").await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert_eq!(b.changes, 1);
    assert_eq!(b.tables.len(), 1);
    assert_eq!(b.tables[0].table.name, "books");
    assert_eq!(
        b.tables[0].upserts,
        vec![vec![
            Value::Int(id),
            Value::Text("Dune".into()),
            Value::Text("Author".into()),
            Value::Null,
            Value::Int(2001),
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ]]
    );
    assert!(b.tables[0].deletes.is_empty());
}

#[tokio::test]
async fn many_changes_to_one_row_become_one_row() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "v0").await;
    for i in 1..=100 {
        rename_book(&db.pool, id, &format!("v{i}")).await;
    }
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert_eq!(b.changes, 101);
    assert_eq!(b.rows(), 1);
    assert_eq!(b.tables[0].upserts[0][1], Value::Text("v100".into()));
}

#[tokio::test]
async fn a_row_that_is_gone_is_read_as_a_delete() {
    let db = TestSqlite::new().await;
    let id = insert_book(&db.pool, "brief").await;
    delete_book(&db.pool, id).await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    assert!(b.tables[0].upserts.is_empty());
    assert_eq!(b.tables[0].deletes, vec![id]);
}

#[tokio::test]
async fn tables_come_in_the_table_list_order() {
    let db = TestSqlite::new().await;
    let book = insert_book(&db.pool, "b").await;
    let member = insert_member(&db.pool, "m@example.com").await;
    insert_loan(&db.pool, book, member).await;
    rename_book(&db.pool, book, "b2").await; // books changes again after loans
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let names: Vec<_> = b.tables.iter().map(|t| t.table.name).collect();
    assert_eq!(names, ["books", "members", "loans"]);
}

#[tokio::test]
async fn batches_stop_at_the_limit_and_continue_without_gaps() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 1203).await;
    let first = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let second = batch(
        changes::read_batch(&db.pool, &after(&first), 500)
            .await
            .unwrap(),
    );
    let third = batch(
        changes::read_batch(&db.pool, &after(&second), 500)
            .await
            .unwrap(),
    );
    assert_eq!(
        (first.changes, second.changes, third.changes),
        (500, 500, 203)
    );
    assert_eq!(
        changes::read_batch(&db.pool, &after(&third), 500)
            .await
            .unwrap(),
        Read::Empty
    );
    let ids: Vec<i64> = [&first, &second, &third]
        .iter()
        .flat_map(|b| {
            b.tables[0]
                .upserts
                .iter()
                .map(|r| tables::row_id(r).unwrap())
        })
        .collect();
    assert_eq!(ids, (1..=1203).collect::<Vec<_>>());
}

#[tokio::test]
async fn a_checkpoint_whose_row_is_missing_is_invalid() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "x").await;
    let gone = Checkpoint {
        epoch: 1,
        last_seq: 999,
        last_token: Some(vec![1; 8]),
    };
    assert_eq!(
        changes::read_batch(&db.pool, &gone, 500).await.unwrap(),
        Read::Invalid
    );
}

#[tokio::test]
async fn a_checkpoint_is_valid_only_with_the_same_token() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "x").await;
    let b = batch(changes::read_batch(&db.pool, &START, 500).await.unwrap());
    let wrong = Checkpoint {
        epoch: 1,
        last_seq: b.last_seq,
        last_token: Some(vec![0; 8]),
    };
    assert_eq!(
        changes::read_batch(&db.pool, &wrong, 500).await.unwrap(),
        Read::Invalid
    );
    let none = Checkpoint {
        epoch: 1,
        last_seq: b.last_seq,
        last_token: None,
    };
    assert_eq!(
        changes::read_batch(&db.pool, &none, 500).await.unwrap(),
        Read::Invalid
    );
    assert_eq!(
        changes::read_batch(&db.pool, &after(&b), 500)
            .await
            .unwrap(),
        Read::Empty
    );
}

#[tokio::test]
async fn an_unknown_table_is_an_error_never_skipped() {
    let db = TestSqlite::new().await;
    sqlx::query("INSERT INTO change_log (table_name, row_id, op) VALUES ('ghosts', 1, 'insert')")
        .execute(&db.pool)
        .await
        .unwrap();
    let err = changes::read_batch(&db.pool, &START, 500)
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("ghosts"), "{err:#}");
}

#[tokio::test]
async fn a_snapshot_of_a_new_database_is_empty_at_zero() {
    let db = TestSqlite::new().await;
    let s = changes::read_snapshot(&db.pool).await.unwrap();
    assert_eq!((s.seq, s.token), (0, None));
    assert_eq!(s.tables.len(), tables::TABLES.len());
    assert!(
        s.tables
            .iter()
            .all(|t| t.upserts.is_empty() && t.deletes.is_empty())
    );
}

#[tokio::test]
async fn a_snapshot_has_every_row_and_the_newest_log_position() {
    let db = TestSqlite::new().await;
    insert_book(&db.pool, "a").await;
    insert_book(&db.pool, "b").await;
    insert_member(&db.pool, "m@example.com").await;
    let s = changes::read_snapshot(&db.pool).await.unwrap();
    let (seq, token): (i64, Vec<u8>) =
        sqlx::query_as("SELECT seq, token FROM change_log ORDER BY seq DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!((s.seq, s.token.clone()), (seq, Some(token)));
    assert_eq!(s.tables[0].upserts.len(), 2);
    assert_eq!(s.tables[1].upserts.len(), 1);
    assert_eq!(
        s.tables[0].upserts,
        changes::read_all(&db.pool, &tables::TABLES[0])
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn pruning_keeps_the_checkpoint_row() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 3).await;
    let newest: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(changes::prune(&db.pool, newest).await.unwrap(), 2);
    let left: Vec<i64> = sqlx::query_scalar("SELECT seq FROM change_log")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(left, vec![newest]);
}

#[tokio::test]
async fn pending_counts_what_is_left_and_its_age() {
    let db = TestSqlite::new().await;
    insert_books(&db.pool, 3).await;
    let first_ms: i64 = sqlx::query_scalar("SELECT MIN(created_at) FROM change_log")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let newest: i64 = sqlx::query_scalar("SELECT MAX(seq) FROM change_log")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let all = changes::pending(&db.pool, 0).await.unwrap();
    assert_eq!((all.count, all.oldest_ms), (3, Some(first_ms)));
    let none = changes::pending(&db.pool, newest).await.unwrap();
    assert_eq!((none.count, none.oldest_ms), (0, None));
}

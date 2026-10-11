//! The Postgres side of the worker (spec §6.2, §6.5, §6.6), on a real Postgres.
#![allow(clippy::unwrap_used)]

mod common;

use common::{TestPg, connect_mirror};
use sync_worker::changes::{Batch, Checkpoint, Snapshot, TableChanges};
use sync_worker::mirror::{ConnectError, Mirror, ReplaceReport};
use sync_worker::tables::{self, Row, Table, Value};

fn books() -> &'static Table {
    tables::by_name("books").unwrap()
}

fn loans() -> &'static Table {
    tables::by_name("loans").unwrap()
}

fn book(id: i64, title: &str) -> Row {
    vec![
        Value::Int(id),
        Value::Text(title.into()),
        Value::Text("A".into()),
        Value::Null,
        Value::Null,
        Value::Int(1),
        Value::Int(10),
        Value::Int(20),
    ]
}

fn batch(last_seq: u8, tables: Vec<TableChanges>) -> Batch {
    Batch {
        changes: 1,
        last_seq: i64::from(last_seq),
        last_token: vec![last_seq; 8],
        tables,
    }
}

fn books_changes(upserts: Vec<Row>, deletes: Vec<i64>) -> TableChanges {
    TableChanges {
        table: books(),
        upserts,
        deletes,
    }
}

#[tokio::test]
async fn connecting_creates_the_schema_and_takes_the_node_lock() {
    let pg = TestPg::new().await;
    let first = Mirror::connect(&pg.options, "node1").await.unwrap();
    let mut conn = pg.connect().await;
    for table in [
        "books",
        "members",
        "loans",
        "sync_checkpoints",
        "sync_archive",
    ] {
        let found: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(table)
            .fetch_one(&mut conn)
            .await
            .unwrap();
        assert_eq!(
            found.as_deref(),
            Some(table),
            "connect should have created {table}"
        );
    }
    drop(conn);
    match Mirror::connect(&pg.options, "node1").await {
        Err(ConnectError::Locked(node)) => assert_eq!(node, "node1"),
        other => panic!("expected Locked, got {:?}", other.err()),
    }
    let other_node = Mirror::connect(&pg.options, "node2").await.unwrap();
    first.close().await;
    let again = connect_mirror(&pg, "node1").await; // closing released the lock
    other_node.close().await;
    again.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_new_node_has_no_checkpoint() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    assert_eq!(m.load_checkpoint().await.unwrap(), None);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_batch_upserts_deletes_and_moves_the_checkpoint() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    m.apply_batch(
        &batch(
            3,
            vec![books_changes(vec![book(1, "a"), book(2, "b")], vec![])],
        ),
        1,
    )
    .await
    .unwrap();
    assert_eq!(
        m.read_all(books()).await.unwrap(),
        vec![book(1, "a"), book(2, "b")]
    );
    m.apply_batch(
        &batch(5, vec![books_changes(vec![book(1, "a2")], vec![2])]),
        1,
    )
    .await
    .unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), vec![book(1, "a2")]);
    assert_eq!(
        m.load_checkpoint().await.unwrap(),
        Some(Checkpoint {
            epoch: 1,
            last_seq: 5,
            last_token: Some(vec![5; 8])
        })
    );
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn applying_the_same_batch_twice_changes_nothing() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let b = batch(4, vec![books_changes(vec![book(1, "a")], vec![9])]);
    m.apply_batch(&b, 1).await.unwrap();
    let rows = m.read_all(books()).await.unwrap();
    let checkpoint = m.load_checkpoint().await.unwrap();
    m.apply_batch(&b, 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    assert_eq!(m.load_checkpoint().await.unwrap(), checkpoint);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn nulls_and_unusual_text_arrive_unchanged() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let long = "x".repeat(2000);
    let rows = vec![
        vec![
            Value::Int(1),
            Value::Text("It's \"quoted\" 📚".into()),
            Value::Text("Ünïcode".into()),
            Value::Null,
            Value::Null,
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ],
        // Empty text and zero are values, not NULL.
        vec![
            Value::Int(2),
            Value::Text(long),
            Value::Text(String::new()),
            Value::Text(String::new()),
            Value::Int(0),
            Value::Int(1),
            Value::Int(0),
            Value::Int(0),
        ],
    ];
    let loan = vec![
        Value::Int(7),
        Value::Int(1),
        Value::Int(2),
        Value::Int(100),
        Value::Int(200),
        Value::Null,
    ];
    let b = batch(
        2,
        vec![
            books_changes(rows.clone(), vec![]),
            TableChanges {
                table: loans(),
                upserts: vec![loan.clone()],
                deletes: vec![],
            },
        ],
    );
    m.apply_batch(&b, 1).await.unwrap();
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    assert_eq!(m.read_all(loans()).await.unwrap(), vec![loan]);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn nodes_do_not_see_each_others_rows() {
    let pg = TestPg::new().await;
    let mut one = connect_mirror(&pg, "node1").await;
    let mut two = connect_mirror(&pg, "node2").await;
    one.apply_batch(
        &batch(1, vec![books_changes(vec![book(1, "one")], vec![])]),
        1,
    )
    .await
    .unwrap();
    two.apply_batch(
        &batch(1, vec![books_changes(vec![book(1, "two")], vec![])]),
        1,
    )
    .await
    .unwrap();
    assert_eq!(one.read_all(books()).await.unwrap(), vec![book(1, "one")]);
    assert_eq!(two.read_all(books()).await.unwrap(), vec![book(1, "two")]);
    // A full copy of node1 leaves node2 alone.
    let empty = Snapshot {
        seq: 0,
        token: None,
        tables: vec![],
    };
    one.replace_all(&empty, 1).await.unwrap();
    assert_eq!(two.read_all(books()).await.unwrap(), vec![book(1, "two")]);
    one.close().await;
    two.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_archives_the_old_rows_and_bumps_the_epoch() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    m.apply_batch(
        &batch(
            3,
            vec![books_changes(vec![book(1, "old"), book(2, "gone")], vec![])],
        ),
        1,
    )
    .await
    .unwrap();
    let snapshot = Snapshot {
        seq: 9,
        token: Some(vec![9; 8]),
        tables: vec![books_changes(vec![book(1, "new")], vec![])],
    };
    let report = m.replace_all(&snapshot, 1).await.unwrap();
    assert_eq!(
        report,
        ReplaceReport {
            epoch: 2,
            archived: 2,
            copied: 1
        }
    );
    assert_eq!(m.read_all(books()).await.unwrap(), vec![book(1, "new")]);
    assert_eq!(
        m.load_checkpoint().await.unwrap(),
        Some(Checkpoint {
            epoch: 2,
            last_seq: 9,
            last_token: Some(vec![9; 8])
        })
    );
    let mut conn = pg.connect().await;
    let archived: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT epoch, table_name, row_data->>'title' FROM sync_archive WHERE node_id = 'node1' ORDER BY 3",
    )
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(
        archived,
        vec![
            (1, "books".into(), "gone".into()),
            (1, "books".into(), "old".into())
        ]
    );
    drop(conn);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_larger_than_one_insert_chunk() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let rows: Vec<Row> = (1..=2500).map(|i| book(i, &format!("bulk {i}"))).collect();
    let snapshot = Snapshot {
        seq: 2500,
        token: Some(vec![7; 8]),
        tables: vec![books_changes(rows.clone(), vec![])],
    };
    let report = m.replace_all(&snapshot, 0).await.unwrap();
    assert_eq!(
        report,
        ReplaceReport {
            epoch: 1,
            archived: 0,
            copied: 2500
        }
    );
    assert_eq!(m.read_all(books()).await.unwrap(), rows);
    m.close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_full_copy_of_an_empty_node_sits_at_zero() {
    let pg = TestPg::new().await;
    let mut m = connect_mirror(&pg, "node1").await;
    let empty = Snapshot {
        seq: 0,
        token: None,
        tables: vec![],
    };
    assert_eq!(
        m.replace_all(&empty, 0).await.unwrap(),
        ReplaceReport {
            epoch: 1,
            archived: 0,
            copied: 0
        }
    );
    assert_eq!(
        m.load_checkpoint().await.unwrap(),
        Some(Checkpoint {
            epoch: 1,
            last_seq: 0,
            last_token: None
        })
    );
    m.close().await;
    pg.cleanup().await;
}

//! Sync cycles end to end, on a real SQLite file and a real Postgres
//! (spec §6.3–6.5, §11).
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use sqlx::SqlitePool;
use sync_worker::changes::{self, Checkpoint, Read};
use sync_worker::mirror;
use sync_worker::sync::{Outcome, Syncer};

const NODE: &str = "node1";

async fn syncer(pool: &SqlitePool, pg: &TestPg, batch_size: usize) -> Syncer {
    Syncer::new(pool.clone(), connect_mirror(pg, NODE).await, batch_size)
}

/// Run cycles until one is Idle; return the outcomes before it.
async fn drain(s: &mut Syncer) -> Vec<Outcome> {
    let mut outcomes = Vec::new();
    loop {
        let outcome = s.cycle().await.unwrap();
        if outcome == Outcome::Idle {
            return outcomes;
        }
        outcomes.push(outcome);
        assert!(outcomes.len() < 100, "never became idle: {outcomes:?}");
    }
}

async fn log_size(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM change_log")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn max_seq(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT MAX(seq) FROM change_log")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn the_first_cycle_copies_what_is_already_there() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    for title in ["a", "b", "c"] {
        insert_book(&db.pool, title).await;
    }
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Copied {
            resync: false,
            epoch: 1,
            archived: 0,
            copied: 3
        }
    );
    assert_eq!(s.cycle().await.unwrap(), Outcome::Idle);
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_new_empty_node_starts_at_zero_and_then_ships() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Copied {
            resync: false,
            epoch: 1,
            archived: 0,
            copied: 0
        }
    );
    assert_eq!(
        s.checkpoint(),
        Some(&Checkpoint {
            epoch: 1,
            last_seq: 0,
            last_token: None
        })
    );
    insert_book(&db.pool, "first").await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Shipped {
            changes: 1,
            rows: 1,
            full: false
        }
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn inserts_updates_and_deletes_reach_postgres() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    let book = insert_book(&db.pool, "b").await;
    let member = insert_member(&db.pool, "m@example.com").await;
    insert_loan(&db.pool, book, member).await;
    rename_book(&db.pool, book, "b renamed").await;
    // Deleting the member cascades to the loan.
    sqlx::query("DELETE FROM members WHERE id = ?")
        .bind(member)
        .execute(&db.pool)
        .await
        .unwrap();
    insert_member(&db.pool, "n@example.com").await;
    drain(&mut s).await;
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn repeated_updates_ship_as_one_row() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    let id = insert_book(&db.pool, "v0").await;
    s.cycle().await.unwrap();
    for i in 1..=100 {
        rename_book(&db.pool, id, &format!("v{i}")).await;
    }
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Shipped {
            changes: 100,
            rows: 1,
            full: false
        }
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn more_changes_than_a_batch_ship_in_several_batches() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_books(&db.pool, 1203).await;
    assert_eq!(
        drain(&mut s).await,
        vec![
            Outcome::Shipped {
                changes: 500,
                rows: 500,
                full: true
            },
            Outcome::Shipped {
                changes: 500,
                rows: 500,
                full: true
            },
            Outcome::Shipped {
                changes: 203,
                rows: 203,
                full: false
            },
        ]
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    assert_eq!(
        log_size(&db.pool).await,
        1,
        "only the checkpoint row is kept"
    );
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_crash_between_commit_and_prune_ships_nothing_twice() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    insert_book(&db.pool, "b").await;
    // Ship by hand and "die" before pruning.
    let mut mirror = s.into_mirror();
    let checkpoint = mirror.load_checkpoint().await.unwrap().unwrap();
    let Read::Batch(batch) = changes::read_batch(&db.pool, &checkpoint, 500)
        .await
        .unwrap()
    else {
        panic!("expected a batch");
    };
    mirror.apply_batch(&batch, checkpoint.epoch).await.unwrap();
    // A fresh worker picks up from Postgres's checkpoint.
    let mut s = Syncer::new(db.pool.clone(), mirror, 500);
    assert_eq!(s.cycle().await.unwrap(), Outcome::Idle);
    insert_book(&db.pool, "c").await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Shipped {
            changes: 1,
            rows: 1,
            full: false
        }
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    assert_eq!(log_size(&db.pool).await, 1);
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn restoring_an_older_backup_resyncs_and_archives_the_lost_rows() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "kept").await;
    s.cycle().await.unwrap();
    let backup = db.backup().await; // the newest backup S3 has
    insert_book(&db.pool, "lost").await;
    s.cycle().await.unwrap(); // Postgres got "lost", the backup did not
    // The node dies; a replacement is restored from the backup.
    s.into_mirror().close().await;
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert_eq!(
        restored.cycle().await.unwrap(),
        Outcome::Copied {
            resync: true,
            epoch: 2,
            archived: 2,
            copied: 1
        }
    );
    assert_in_sync(&backup.pool, &pg, NODE).await;
    let mut conn = pg.connect().await;
    let archived: Vec<String> = sqlx::query_scalar(
        "SELECT row_data->>'title' FROM sync_archive WHERE node_id = $1 AND epoch = 1 ORDER BY 1",
    )
    .bind(NODE)
    .fetch_all(&mut conn)
    .await
    .unwrap();
    assert_eq!(archived, vec!["kept".to_owned(), "lost".to_owned()]);
    drop(conn);
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn new_writes_that_reuse_sequence_numbers_are_detected() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "kept").await;
    s.cycle().await.unwrap();
    let backup = db.backup().await;
    insert_book(&db.pool, "lost").await;
    s.cycle().await.unwrap();
    let lost_seq = max_seq(&db.pool).await;
    s.into_mirror().close().await;
    // The restored node takes a write before its worker runs.
    insert_book(&backup.pool, "new after restore").await;
    assert_eq!(
        max_seq(&backup.pool).await,
        lost_seq,
        "the restored database reused the number"
    );
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert!(matches!(
        restored.cycle().await.unwrap(),
        Outcome::Copied { resync: true, .. }
    ));
    assert_in_sync(&backup.pool, &pg, NODE).await;
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn restoring_a_newer_backup_carries_on_without_a_resync() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "shipped").await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "not shipped yet").await;
    let backup = db.backup().await; // Litestream was ahead of the worker
    s.into_mirror().close().await;
    let mut restored = syncer(&backup.pool, &pg, 500).await;
    assert_eq!(
        restored.cycle().await.unwrap(),
        Outcome::Shipped {
            changes: 1,
            rows: 1,
            full: false
        }
    );
    assert_in_sync(&backup.pool, &pg, NODE).await;
    restored.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_reused_row_id_ends_with_the_new_values() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    let b = insert_book(&db.pool, "b").await;
    s.cycle().await.unwrap();
    // Delete and re-insert in one batch: SQLite hands out the same id again.
    delete_book(&db.pool, b).await;
    assert_eq!(insert_book(&db.pool, "b2").await, b);
    s.cycle().await.unwrap();
    assert_in_sync(&db.pool, &pg, NODE).await;
    // The same across two batches.
    delete_book(&db.pool, b).await;
    s.cycle().await.unwrap();
    assert_eq!(insert_book(&db.pool, "b3").await, b);
    s.cycle().await.unwrap();
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn postgres_restored_to_an_older_point_gets_a_full_copy() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "a").await;
    s.cycle().await.unwrap();
    // Postgres's checkpoint now: the point its backup will go back to.
    let mut conn = pg.connect().await;
    let old = mirror::load_checkpoint(&mut conn, NODE)
        .await
        .unwrap()
        .unwrap();
    insert_book(&db.pool, "b").await;
    s.cycle().await.unwrap(); // ships "b" and prunes the change_log row `old` names
    s.into_mirror().close().await;
    // Postgres is restored from that backup: old checkpoint, no "b".
    sqlx::query("UPDATE sync_checkpoints SET last_seq = $2, last_token = $3 WHERE node_id = $1")
        .bind(NODE)
        .bind(old.last_seq)
        .bind(old.last_token.clone())
        .execute(&mut conn)
        .await
        .unwrap();
    sqlx::query("DELETE FROM books WHERE node_id = $1 AND title = 'b'")
        .bind(NODE)
        .execute(&mut conn)
        .await
        .unwrap();
    drop(conn);
    let mut s = syncer(&db.pool, &pg, 500).await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Copied {
            resync: true,
            epoch: 2,
            archived: 1,
            copied: 2
        }
    );
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn an_unknown_table_stops_the_cycle_without_moving_the_checkpoint() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    insert_book(&db.pool, "x").await;
    sqlx::query("INSERT INTO change_log (table_name, row_id, op) VALUES ('ghosts', 1, 'insert')")
        .execute(&db.pool)
        .await
        .unwrap();
    let mut conn = pg.connect().await;
    let before = mirror::load_checkpoint(&mut conn, NODE).await.unwrap();
    let err = s.cycle().await.unwrap_err();
    assert!(format!("{err:#}").contains("ghosts"), "{err:#}");
    assert_eq!(
        mirror::load_checkpoint(&mut conn, NODE).await.unwrap(),
        before
    );
    drop(conn);
    s.into_mirror().close().await;
    pg.cleanup().await;
}

#[tokio::test]
async fn a_failed_prune_keeps_the_outcome_and_is_counted() {
    let (db, pg) = (TestSqlite::new().await, TestPg::new().await);
    let mut s = syncer(&db.pool, &pg, 500).await;
    s.cycle().await.unwrap();
    sqlx::query(
        "CREATE TRIGGER block_prune BEFORE DELETE ON change_log \
         BEGIN SELECT RAISE(ABORT, 'prune blocked'); END",
    )
    .execute(&db.pool)
    .await
    .unwrap();
    // Pruning keeps the checkpoint row itself, so two changes are needed
    // for it to have anything to delete.
    insert_book(&db.pool, "a").await;
    insert_book(&db.pool, "b").await;
    assert_eq!(
        s.cycle().await.unwrap(),
        Outcome::Shipped {
            changes: 2,
            rows: 2,
            full: false
        }
    );
    assert_eq!(s.take_prune_failures(), 1);
    assert_eq!(s.take_prune_failures(), 0, "reading resets the count");
    assert_in_sync(&db.pool, &pg, NODE).await;
    s.into_mirror().close().await;
    pg.cleanup().await;
}

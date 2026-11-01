//! Readers: a pinned moment that stays put while the writer moves on.

use std::sync::Arc;

use crate::error::Error;
use crate::exec::{Database, Output, Reader};
use crate::storage::MemStorage;
use crate::time::{ManualClock, format_timestamp};
use crate::trace::NoopTracer;
use crate::types::Value;

const SECOND: i64 = 1_000_000;
const START: i64 = 1_793_534_400_000_000;

fn open() -> (Database, Arc<ManualClock>) {
    let mut db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        // A small cache, so a reader's pages push the writer's out to the log.
        8,
        Arc::new(NoopTracer),
    )
    .unwrap();
    let clock = Arc::new(ManualClock::new(START));
    db.set_clock(clock.clone());
    for sql in [
        "CREATE TABLE t (id INT PRIMARY KEY, n INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ] {
        db.execute(sql).unwrap();
    }
    (db, clock)
}

fn run(db: &mut Database, sql: &str) {
    db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn ns(reader: &Reader) -> Vec<i64> {
    let Output::Rows { rows, .. } = reader.query("SELECT n FROM t").unwrap() else {
        panic!()
    };
    rows.iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect()
}

#[test]
fn a_reader_pins_the_latest_commit() {
    let (db, _) = open();
    let reader = db.reader().unwrap();
    assert_eq!(reader.txn(), 2);
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn later_commits_are_invisible() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 0 WHERE id = 1");
    run(&mut db, "DELETE FROM t WHERE id = 2");
    run(&mut db, "INSERT INTO t VALUES (4, 40)");
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn an_open_transaction_is_invisible() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = n + 1");
    run(&mut db, "DELETE FROM t WHERE id = 3");
    assert_eq!(ns(&reader), [10, 20, 30]);
    run(&mut db, "COMMIT");
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn a_reader_taken_during_a_transaction_sees_the_last_commit() {
    let (mut db, _) = open();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 0");
    let reader = db.reader().unwrap();
    assert_eq!(reader.txn(), 2);
    assert_eq!(ns(&reader), [10, 20, 30]);
    run(&mut db, "ROLLBACK");
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn a_rollback_changes_nothing_for_a_reader() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "BEGIN");
    run(&mut db, "INSERT INTO t VALUES (9, 90)");
    run(&mut db, "DROP TABLE t");
    run(&mut db, "ROLLBACK");
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn a_dropped_table_stays_readable() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "BEGIN");
    run(&mut db, "DROP TABLE t");
    assert_eq!(ns(&reader), [10, 20, 30], "while the drop is uncommitted");
    run(&mut db, "COMMIT");
    assert_eq!(ns(&reader), [10, 20, 30], "and after it");
}

#[test]
fn a_new_table_stays_out_of_sight() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "CREATE TABLE later (a INT)");
    assert!(matches!(
        reader.query("SELECT * FROM later"),
        Err(Error::Query(msg)) if msg.contains("no table later at txn 2")
    ));
}

#[test]
fn a_checkpoint_changes_nothing_for_a_reader() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 0");
    db.checkpoint().unwrap();
    assert_eq!(ns(&reader), [10, 20, 30]);
}

#[test]
fn indexes_change_nothing_for_a_reader() {
    let (mut db, _) = open();
    run(&mut db, "CREATE INDEX by_n ON t (n)");
    let reader = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 21 WHERE id = 2");
    let Output::Rows { rows, .. } = reader.query("SELECT id FROM t WHERE n = 20").unwrap() else {
        panic!()
    };
    assert_eq!(rows, [[Value::Int(2)]]);
}

#[test]
fn refresh_moves_up_to_the_latest_commit() {
    let (mut db, _) = open();
    let mut reader = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 0");
    reader.refresh().unwrap();
    assert_eq!(reader.txn(), 3);
    assert_eq!(ns(&reader), [0, 0, 0]);
}

#[test]
fn readers_can_differ() {
    let (mut db, _) = open();
    let first = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 0 WHERE id = 1");
    let second = db.reader().unwrap();
    run(&mut db, "UPDATE t SET n = 0 WHERE id = 2");
    assert_eq!(ns(&first), [10, 20, 30]);
    assert_eq!(ns(&second), [0, 20, 30]);
}

#[test]
fn a_reader_only_reads() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    for sql in [
        "INSERT INTO t VALUES (9, 9)",
        "UPDATE t SET n = 0",
        "DELETE FROM t",
        "DROP TABLE t",
        "BEGIN",
        "VACUUM HISTORY BEFORE '2026-01-01'",
    ] {
        assert!(
            matches!(reader.query(sql), Err(Error::Query(msg)) if msg.contains("only reads")),
            "{sql}"
        );
    }
    assert_eq!(db.latest_txn().unwrap(), 2);
}

#[test]
fn a_reader_already_has_its_moment() {
    let (db, _) = open();
    let reader = db.reader().unwrap();
    for sql in ["SELECT * FROM t AS OF TXN 1", "SELECT * FROM t HISTORY"] {
        assert!(
            matches!(reader.query(sql), Err(Error::Query(msg)) if msg.contains("snapshot")),
            "{sql}"
        );
    }
}

#[test]
fn a_reader_can_explain_a_select() {
    let (db, _) = open();
    let reader = db.reader().unwrap();
    let Output::Rows { rows, .. } = reader.query("EXPLAIN SELECT * FROM t").unwrap() else {
        panic!()
    };
    assert_eq!(rows[0][0].to_string(), "FULL SCAN t AS OF TXN 2");
}

#[test]
fn a_reader_doesnt_disturb_the_writer() {
    let (mut db, _) = open();
    let reader = db.reader().unwrap();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 1");
    assert!(reader.query("SELECT * FROM missing").is_err());
    run(&mut db, "UPDATE t SET n = 2");
    run(&mut db, "COMMIT");
    db.check().unwrap();
    assert_eq!(db.latest_txn().unwrap(), 3);
}

#[test]
fn vacuum_cant_pass_a_reader() {
    let (mut db, clock) = open();
    let reader = db.reader().unwrap();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 0");
    clock.advance(SECOND);
    let now = format_timestamp(START + 2 * SECOND);
    let vacuum = format!("VACUUM HISTORY BEFORE '{now}'");
    assert!(matches!(
        db.execute(&vacuum),
        Err(Error::Query(msg)) if msg.contains("reader") && msg.contains("txn 2")
    ));
    assert_eq!(ns(&reader), [10, 20, 30]);

    drop(reader);
    run(&mut db, &vacuum);
    assert_eq!(db.check().unwrap().old_versions, 0);
}

#[test]
fn vacuum_up_to_a_readers_moment_is_fine() {
    let (mut db, clock) = open();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 1");
    let reader = db.reader().unwrap();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 2");
    let at_reader = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{at_reader}'"));
    assert_eq!(ns(&reader), [1, 1, 1]);
}

#[test]
fn a_refreshed_reader_lets_vacuum_go() {
    let (mut db, clock) = open();
    let mut reader = db.reader().unwrap();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 0");
    reader.refresh().unwrap();
    let now = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    assert_eq!(ns(&reader), [0, 0, 0]);
}

#[test]
fn a_reader_outlives_its_database_only_to_say_so() {
    let (db, _) = open();
    let reader = db.reader().unwrap();
    db.close().unwrap();
    assert!(matches!(
        reader.query("SELECT * FROM t"),
        Err(Error::Query(msg)) if msg.contains("closed")
    ));
}

#[test]
fn closing_frees_the_file_even_with_a_reader_left() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (a INT)").unwrap();
    let kept = db.reader().unwrap();
    db.close().unwrap();
    Database::open(&path).unwrap().close().unwrap();
    drop(kept);

    let db = Database::open(&path).unwrap();
    let kept = db.reader().unwrap();
    drop(db); // without close, as a crash would
    Database::open(&path).unwrap().close().unwrap();
    drop(kept);
}

#[test]
fn a_reader_can_move_to_another_thread() {
    fn send<T: Send + Sync>(_: &T) {}
    let (db, _) = open();
    let reader = db.reader().unwrap();
    send(&reader);
    let handle = std::thread::spawn(move || ns(&reader));
    assert_eq!(handle.join().unwrap(), [10, 20, 30]);
}

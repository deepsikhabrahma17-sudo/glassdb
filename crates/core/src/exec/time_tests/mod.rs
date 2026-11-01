//! `AS OF`, `HISTORY` and `VACUUM HISTORY`, driven through SQL. This file has
//! transaction ids and `AS OF`; the rest are in the files beside it.

mod check;
mod history;
mod vacuum;

use std::sync::Arc;

use crate::btree::BTree;
use crate::error::Error;
use crate::exec::{Database, Output};
use crate::mvcc::{COMMITS_ROOT, history_key, split_history_key, stamped, unstamped};
use crate::storage::MemStorage;
use crate::time::{Clock, ManualClock, format_timestamp};
use crate::trace::{Event, NoopTracer, RecordingTracer};
use crate::types::Value;

/// One second, on the test clock.
const SECOND: i64 = 1_000_000;
const START: i64 = 1_793_534_400_000_000; // 2026-11-01 12:00:00 UTC

fn open() -> (Database, Arc<ManualClock>) {
    let mut db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        64,
        Arc::new(NoopTracer),
    )
    .unwrap();
    let clock = Arc::new(ManualClock::new(START));
    db.set_clock(clock.clone());
    (db, clock)
}

fn run(db: &mut Database, sql: &str) -> Output {
    db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        Output::Rows { rows, .. } => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn query_error(db: &mut Database, sql: &str) -> String {
    match db.execute(sql) {
        Err(Error::Query(msg)) => msg,
        other => panic!("{sql}: {other:?}"),
    }
}

fn ints(values: &[i64]) -> Vec<Vec<Value>> {
    values.iter().map(|&v| vec![Value::Int(v)]).collect()
}

/// `t` with two commits: txn 1 makes it, and txn 2 fills it.
fn accounts() -> (Database, Arc<ManualClock>) {
    let (mut db, clock) = open();
    run(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    run(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)");
    (db, clock)
}

fn n_at(db: &mut Database, txn: u64) -> Vec<Vec<Value>> {
    rows(db, &format!("SELECT n FROM t AS OF TXN {txn}"))
}

#[test]
fn a_new_database_is_at_txn_zero() {
    let (mut db, _) = open();
    assert_eq!(db.latest_txn().unwrap(), 0);
    assert!(db.commits(10).unwrap().is_empty());
}

#[test]
fn each_commit_takes_the_next_id() {
    let (mut db, _) = accounts();
    assert_eq!(db.latest_txn().unwrap(), 2);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    assert_eq!(db.latest_txn().unwrap(), 3);
}

#[test]
fn reads_and_empty_writes_take_no_id() {
    let (mut db, _) = accounts();
    rows(&mut db, "SELECT * FROM t");
    rows(&mut db, "SELECT * FROM t AS OF TXN 1");
    run(&mut db, "UPDATE t SET n = 0 WHERE id = 99");
    run(&mut db, "DELETE FROM t WHERE id = 99");
    run(&mut db, "CREATE TABLE IF NOT EXISTS t (a INT)");
    run(&mut db, "DROP TABLE IF EXISTS missing");
    run(&mut db, "BEGIN");
    run(&mut db, "COMMIT");
    assert_eq!(db.latest_txn().unwrap(), 2);
}

#[test]
fn a_rejected_statement_takes_no_id() {
    let (mut db, _) = accounts();
    query_error(&mut db, "INSERT INTO t VALUES (1, 1)");
    assert_eq!(db.latest_txn().unwrap(), 2);
    run(&mut db, "INSERT INTO t VALUES (3, 30)");
    assert_eq!(db.latest_txn().unwrap(), 3);
}

#[test]
fn a_rollback_gives_its_id_back() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 0");
    run(&mut db, "ROLLBACK");
    assert_eq!(db.latest_txn().unwrap(), 2);
    run(&mut db, "UPDATE t SET n = 5 WHERE id = 1");
    assert_eq!(db.latest_txn().unwrap(), 3);
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[5, 20]));
}

#[test]
fn a_transaction_is_one_commit() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    run(&mut db, "UPDATE t SET n = 22 WHERE id = 2");
    run(&mut db, "INSERT INTO t VALUES (3, 33)");
    run(&mut db, "COMMIT");
    assert_eq!(db.latest_txn().unwrap(), 3);
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[11, 22, 33]));
}

#[test]
fn as_of_txn_rewinds_updates_and_deletes() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    run(&mut db, "DELETE FROM t WHERE id = 2"); // 4
    run(&mut db, "INSERT INTO t VALUES (2, 200)"); // 5
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[11, 20]));
    assert_eq!(n_at(&mut db, 4), ints(&[11]));
    assert_eq!(n_at(&mut db, 5), ints(&[11, 200]));
    assert_eq!(
        rows(&mut db, "SELECT n FROM t"),
        ints(&[11, 200]),
        "the present is unchanged"
    );
}

#[test]
fn a_table_is_empty_before_its_first_insert() {
    let (mut db, _) = accounts();
    assert!(n_at(&mut db, 1).is_empty());
}

#[test]
fn a_moved_primary_key_is_a_delete_and_an_insert() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET id = 9 WHERE id = 1"); // 3
    assert_eq!(rows(&mut db, "SELECT id FROM t AS OF TXN 2"), ints(&[1, 2]));
    assert_eq!(rows(&mut db, "SELECT id FROM t AS OF TXN 3"), ints(&[2, 9]));
}

#[test]
fn swapped_primary_keys_rewind() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET id = 3 - id"); // 3: rows 1 and 2 swap places
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[20, 10]));
}

#[test]
fn a_filter_applies_to_the_past_row() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 99 WHERE id = 1"); // 3
    assert_eq!(
        rows(&mut db, "SELECT id FROM t AS OF TXN 2 WHERE n = 10"),
        ints(&[1])
    );
    assert!(rows(&mut db, "SELECT id FROM t AS OF TXN 2 WHERE n = 99").is_empty());
    assert_eq!(
        rows(&mut db, "SELECT n FROM t AS OF TXN 2 WHERE id = 1"),
        ints(&[10])
    );
}

#[test]
fn as_of_ignores_indexes_built_later() {
    let (mut db, _) = accounts();
    run(&mut db, "CREATE INDEX by_n ON t (n)"); // 3
    run(&mut db, "UPDATE t SET n = 99 WHERE id = 1"); // 4
    assert_eq!(
        rows(&mut db, "SELECT id FROM t AS OF TXN 3 WHERE n = 10"),
        ints(&[1]),
        "the index only knows current rows"
    );
    assert!(rows(&mut db, "SELECT id FROM t WHERE n = 10").is_empty());
}

#[test]
fn explain_shows_the_past() {
    let (mut db, _) = accounts();
    run(&mut db, "CREATE INDEX by_n ON t (n)");
    let plan = |db: &mut Database, sql: &str| rows(db, &format!("EXPLAIN {sql}"))[0][0].to_string();
    assert_eq!(
        plan(&mut db, "SELECT * FROM t AS OF TXN 1 WHERE n = 1"),
        "FULL SCAN t AS OF TXN 1"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t AS OF TXN 1 WHERE id = 1"),
        "PRIMARY KEY LOOKUP ON t (id = 1) AS OF TXN 1"
    );
    assert_eq!(
        plan(&mut db, "SELECT * FROM t HISTORY"),
        "FULL SCAN t HISTORY"
    );
}

#[test]
fn the_future_and_unknown_tables_are_refused() {
    let (mut db, _) = accounts();
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 3").contains("latest is 2"));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 0").contains("no table t at txn 0"));
    assert!(
        query_error(&mut db, "SELECT * FROM missing AS OF TXN 1")
            .contains("no table missing at txn 1")
    );
    assert!(query_error(&mut db, "SELECT * FROM missing HISTORY").contains("no such table"));
}

#[test]
fn a_table_made_later_did_not_exist_then() {
    let (mut db, _) = accounts();
    run(&mut db, "CREATE TABLE later (a INT)"); // 3
    assert!(rows(&mut db, "SELECT * FROM later AS OF TXN 3").is_empty());
    assert!(
        query_error(&mut db, "SELECT * FROM later AS OF TXN 2").contains("no table later at txn 2")
    );
}

#[test]
fn as_of_time_finds_the_last_commit_by_then() {
    let (mut db, clock) = accounts();
    let at = |offset: i64| format_timestamp(START + offset);
    clock.advance(10 * SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3, at +10s
    clock.advance(10 * SECOND);
    run(&mut db, "DELETE FROM t WHERE id = 2"); // 4, at +20s

    let n_then =
        |db: &mut Database, time: String| rows(db, &format!("SELECT n FROM t AS OF '{time}'"));
    assert_eq!(n_then(&mut db, at(0)), ints(&[10, 20]));
    assert_eq!(n_then(&mut db, at(9)), ints(&[10, 20]));
    assert_eq!(n_then(&mut db, at(10 * SECOND)), ints(&[11, 20]));
    assert_eq!(n_then(&mut db, at(20 * SECOND - 1)), ints(&[11, 20]));
    assert_eq!(n_then(&mut db, at(20 * SECOND)), ints(&[11]));
    assert_eq!(n_then(&mut db, at(1_000 * SECOND)), ints(&[11]));
}

#[test]
fn a_time_before_the_table_is_refused() {
    let (mut db, _) = accounts();
    let earlier = format_timestamp(START - SECOND);
    assert!(
        query_error(&mut db, &format!("SELECT * FROM t AS OF '{earlier}'"))
            .contains("no table t at txn 0")
    );
}

#[test]
fn commit_times_never_go_backwards() {
    let (mut db, clock) = accounts();
    clock.set(START - 100 * SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    let commits = db.commits(1).unwrap();
    assert_eq!(commits[0].txn, 3);
    assert_eq!(commits[0].time, START, "held at the previous commit's time");
    // Txn 3 still sorts after txn 2, so a lookup by time finds it.
    let now = format_timestamp(START);
    assert_eq!(
        rows(&mut db, &format!("SELECT n FROM t AS OF '{now}'")),
        ints(&[11, 20])
    );
}

#[test]
fn commit_times_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    db.set_clock(Arc::new(ManualClock::new(START)));
    run(&mut db, "CREATE TABLE t (a INT)");
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    let clock = Arc::new(ManualClock::new(START - SECOND));
    db.set_clock(clock.clone());
    assert_eq!(db.latest_txn().unwrap(), 1);
    assert_eq!(db.commits(1).unwrap()[0].time, START);
    run(&mut db, "INSERT INTO t VALUES (1)");
    assert_eq!(db.commits(1).unwrap()[0].time, START, "still no going back");
    assert_eq!(clock.now(), START - SECOND);
}

#[test]
fn old_rows_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    run(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");
    run(&mut db, "UPDATE t SET n = 11");
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(n_at(&mut db, 2), ints(&[10]));
    assert_eq!(n_at(&mut db, 3), ints(&[11]));
    run(&mut db, "UPDATE t SET n = 12");
    assert_eq!(db.latest_txn().unwrap(), 4, "ids carry on");
    db.check().unwrap();
}

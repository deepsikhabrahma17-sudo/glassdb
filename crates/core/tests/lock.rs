//! One open at a time: a database file is locked for as long as it is open.

use glassdb_core::{Database, Error, Output, Value};

fn ids(db: &mut Database) -> Vec<Value> {
    match db.execute("SELECT id FROM t ORDER BY id").unwrap() {
        Output::Rows { rows, .. } => rows.into_iter().map(|mut row| row.remove(0)).collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn is_locked<T>(result: glassdb_core::Result<T>) -> bool {
    matches!(result, Err(Error::Locked))
}

#[test]
fn a_second_open_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock.db");
    let _first = Database::open(&path).unwrap();
    assert!(is_locked(Database::open(&path)));
}

#[test]
fn a_refused_open_leaves_the_log_alone() {
    // Opening empties the log, so without the lock this open would destroy
    // the first one's committed work.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock.db");
    let log = dir.path().join("lock.db-wal");

    let mut first = Database::open(&path).unwrap();
    first
        .execute("CREATE TABLE t (id INT PRIMARY KEY)")
        .unwrap();
    first.execute("INSERT INTO t VALUES (1)").unwrap();
    let before = std::fs::read(&log).unwrap();
    assert!(!before.is_empty());

    assert!(is_locked(Database::open(&path)));
    assert_eq!(std::fs::read(&log).unwrap(), before);

    first.execute("INSERT INTO t VALUES (2)").unwrap();
    drop(first); // a crash
    let mut db = Database::open(&path).unwrap();
    assert_eq!(ids(&mut db), vec![Value::Int(1), Value::Int(2)]);
}

#[test]
fn close_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock.db");
    Database::open(&path).unwrap().close().unwrap();
    Database::open(&path).unwrap().close().unwrap();
}

#[test]
fn a_crash_releases_the_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lock.db");
    drop(Database::open(&path).unwrap());
    Database::open(&path).unwrap().close().unwrap();
}

#[test]
fn different_files_do_not_share_a_lock() {
    let dir = tempfile::tempdir().unwrap();
    let _a = Database::open(dir.path().join("a.db")).unwrap();
    let _b = Database::open(dir.path().join("b.db")).unwrap();
}

#[test]
fn the_message_says_what_is_wrong() {
    assert!(Error::Locked.to_string().contains("locked"));
}

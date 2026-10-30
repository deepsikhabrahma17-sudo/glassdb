//! Tests of the `Database` API itself. SQL behaviour is covered by the
//! `tests/sql/*.test` files.

use std::sync::Arc;

use super::*;
use crate::storage::MemStorage;
use crate::trace::{Event, RecordingTracer};
use crate::types::Value;

fn open() -> (Database, Arc<RecordingTracer>) {
    let tracer = Arc::new(RecordingTracer::default());
    let db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        16,
        tracer.clone(),
    )
    .unwrap();
    tracer.take();
    (db, tracer)
}

#[test]
fn execute_returns_typed_outputs() {
    let (mut db, _) = open();
    assert_eq!(
        db.execute("CREATE TABLE t (a INT, b TEXT)").unwrap(),
        Output::Message("created table t".into())
    );
    assert_eq!(
        db.execute("INSERT INTO t VALUES (1, 'x'), (2, NULL)")
            .unwrap(),
        Output::Inserted(2)
    );
    assert_eq!(
        db.execute("SELECT a, b FROM t WHERE a = 2").unwrap(),
        Output::Rows {
            columns: vec!["a".into(), "b".into()],
            rows: vec![vec![Value::Int(2), Value::Null]],
        }
    );
    assert_eq!(
        db.execute("UPDATE t SET b = 'y'").unwrap(),
        Output::Updated(2)
    );
    assert_eq!(
        db.execute("DELETE FROM t WHERE a = 1").unwrap(),
        Output::Deleted(1)
    );
    let names: Vec<String> = db.tables().unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, vec!["t"]);
}

#[test]
fn outputs_print_as_text() {
    assert_eq!(Output::Inserted(1).to_string(), "inserted 1 row");
    assert_eq!(Output::Deleted(3).to_string(), "deleted 3 rows");
    let rows = Output::Rows {
        columns: vec!["a".into(), "b".into()],
        rows: vec![
            vec![Value::Int(1), Value::Text("x".into())],
            vec![Value::Null, Value::Real(2.0)],
        ],
    };
    assert_eq!(rows.to_string(), "a | b\n1 | x\nNULL | 2.0");
}

#[test]
fn execute_takes_exactly_one_statement() {
    let (mut db, _) = open();
    assert!(db.execute("SELECT 1; SELECT 2").is_err());
    assert!(db.execute("").is_err());
}

#[test]
fn statements_report_start_and_finish_to_the_tracer() {
    let (mut db, tracer) = open();
    db.execute("create table t (a int)").unwrap();
    assert!(db.execute("SELECT * FROM nope").is_err());
    let events: Vec<Event> = tracer
        .take()
        .into_iter()
        .filter(|e| matches!(e, Event::Statement { .. } | Event::StatementDone { .. }))
        .collect();
    assert_eq!(
        events,
        vec![
            Event::Statement {
                sql: "CREATE TABLE t (a INT)".into()
            },
            Event::StatementDone {
                summary: "created table t".into()
            },
            Event::Statement {
                sql: "SELECT * FROM nope".into()
            },
            Event::StatementDone {
                summary: "error: no such table: nope".into()
            },
        ]
    );
}

#[test]
fn check_reports_the_shape_of_the_database() {
    let (mut db, _) = open();
    db.execute("CREATE TABLE a (n INT PRIMARY KEY, t TEXT)")
        .unwrap();
    db.execute("CREATE TABLE b (n INT)").unwrap();
    db.execute("INSERT INTO a VALUES (1, 'x'), (2, 'y')")
        .unwrap();
    db.execute("INSERT INTO b VALUES (7)").unwrap();
    let report = db.check().unwrap();
    assert_eq!(report.tables, 2);
    assert_eq!(report.rows, 3);
    assert_eq!(report.pages, 4, "header, catalog and one page per table");
    assert_eq!(report.free_pages, 0);
    db.execute("DROP TABLE a").unwrap();
    assert_eq!(db.check().unwrap().free_pages, 1);
}

#[test]
fn the_log_file_lives_beside_the_database_until_a_clean_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("my.db");
    let wal = dir.path().join("my.db-wal");

    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (a INT)").unwrap();
    db.execute("INSERT INTO t VALUES (1)").unwrap();
    assert!(wal.exists());
    drop(db); // a crash: no close

    let mut db = Database::open(&path).unwrap();
    assert_eq!(
        db.recovered(),
        3,
        "catalog creation, CREATE TABLE and INSERT"
    );
    assert_eq!(db.execute("SELECT a FROM t").unwrap().to_string(), "a\n1");
    db.close().unwrap();
    assert!(!wal.exists(), "a clean close removes the log");

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.recovered(), 0);
    assert_eq!(db.execute("SELECT a FROM t").unwrap().to_string(), "a\n1");
}

#[test]
fn a_file_that_is_not_a_database_gets_no_log_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, vec![0xFF; 4096]).unwrap();
    assert!(Database::open(&path).is_err());
    assert!(!dir.path().join("notes.txt-wal").exists());
}

#[test]
fn checkpoint_moves_the_log_into_the_main_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("my.db");
    let mut db = Database::open(&path).unwrap();
    db.execute("CREATE TABLE t (a INT)").unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().len(),
        0,
        "still only in the log"
    );
    db.checkpoint().unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().len(), 3 * 4096);
}

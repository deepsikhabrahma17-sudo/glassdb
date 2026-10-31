//! Indexes through the `Database` API. SQL behaviour is in `indexes.test`.

use std::sync::Arc;

use super::*;
use crate::btree::MAX_KEY;
use crate::storage::MemStorage;
use crate::trace::{Event, RecordingTracer};

struct Disks {
    db: MemStorage,
    log: MemStorage,
    tracer: Arc<RecordingTracer>,
}

impl Disks {
    fn new() -> Disks {
        Disks {
            db: MemStorage::new(),
            log: MemStorage::new(),
            tracer: Arc::new(RecordingTracer::default()),
        }
    }

    fn open(&self) -> Database {
        Database::open_with(
            Box::new(self.db.clone()),
            Box::new(self.log.clone()),
            8,
            self.tracer.clone(),
        )
        .unwrap()
    }
}

/// `t` holding rows (1, 10, 'a') .. (5, 50, 'e'), with an index on `n`.
fn seeded(disks: &Disks) -> Database {
    let mut db = disks.open();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT)")
        .unwrap();
    db.execute(
        "INSERT INTO t VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, 40, 'd'), (5, 50, 'e')",
    )
    .unwrap();
    db.execute("CREATE INDEX by_n ON t (n)").unwrap();
    db
}

fn err(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

fn rows(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap().to_string()
}

fn explain(db: &mut Database, sql: &str) -> Vec<String> {
    match db.execute(&format!("EXPLAIN {sql}")).unwrap() {
        Output::Rows { columns, rows } => {
            assert_eq!(columns, vec!["plan"]);
            rows.iter().map(|row| row[0].to_string()).collect()
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// An index's tree, for changing entries behind the engine's back.
fn index_tree(db: &mut Database, name: &str) -> BTree {
    let index = db
        .indexes()
        .unwrap()
        .into_iter()
        .find(|i| i.name == name)
        .expect("the index exists");
    BTree::open(index.root)
}

#[test]
fn explain_shows_each_step_and_runs_nothing() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    assert_eq!(
        explain(
            &mut db,
            "SELECT s FROM t WHERE n > 15 ORDER BY s DESC LIMIT 2 OFFSET 1"
        ),
        vec![
            "INDEX SCAN by_n ON t (n > 15)",
            "FILTER n > 15",
            "SORT s DESC",
            "OFFSET 1",
            "LIMIT 2",
        ]
    );
    assert_eq!(explain(&mut db, "SELECT * FROM t"), vec!["FULL SCAN t"]);
    assert_eq!(explain(&mut db, "SELECT 1"), vec!["ONE ROW (no table)"]);
    assert_eq!(
        explain(&mut db, "UPDATE t SET s = 'x' WHERE id = 2"),
        vec![
            "PRIMARY KEY LOOKUP ON t (id = 2)",
            "FILTER id = 2",
            "UPDATE t",
        ]
    );
    assert_eq!(
        explain(&mut db, "DELETE FROM t"),
        vec!["FULL SCAN t", "DELETE FROM t"]
    );
    assert!(
        rows(&mut db, "SELECT id FROM t WHERE id = 2").contains('2'),
        "EXPLAIN UPDATE and DELETE must not run"
    );
    assert_eq!(rows(&mut db, "SELECT s FROM t WHERE id = 2"), "s\nb");
}

#[test]
fn explain_checks_the_statement() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    assert!(err(&mut db, "EXPLAIN SELECT * FROM missing").contains("no such table"));
    assert!(err(&mut db, "EXPLAIN SELECT nope FROM t").contains("no such column"));
    assert!(err(&mut db, "EXPLAIN DELETE FROM t WHERE nope = 1").contains("no such column"));
}

#[test]
fn the_tracer_gets_the_plan() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    disks.tracer.take();
    db.execute("SELECT * FROM t WHERE n = 20").unwrap();
    let steps: Vec<_> = disks
        .tracer
        .take()
        .into_iter()
        .filter_map(|event| match event {
            Event::Plan { steps } => Some(steps),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        vec![vec![
            "INDEX SCAN by_n ON t (n = 20)".to_string(),
            "FILTER n = 20".to_string()
        ]]
    );
}

#[test]
fn every_write_keeps_the_index_in_step() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    db.execute("INSERT INTO t VALUES (6, 25, 'f')").unwrap();
    db.execute("UPDATE t SET n = 26 WHERE id = 1").unwrap();
    db.execute("UPDATE t SET id = 100 WHERE id = 3").unwrap();
    db.execute("DELETE FROM t WHERE n = 50").unwrap();
    assert_eq!(db.check().unwrap().rows, 5);

    assert_eq!(
        rows(&mut db, "SELECT id, n FROM t WHERE n >= 25 AND n <= 30"),
        "id | n\n1 | 26\n6 | 25\n100 | 30"
    );
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE n = 10"), "id");
}

#[test]
fn rows_come_back_in_table_order() {
    let disks = Disks::new();
    let mut db = disks.open();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 30), (2, 10), (3, 20), (4, 10)")
        .unwrap();
    db.execute("CREATE INDEX by_n ON t (n)").unwrap();
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE n > 0"),
        "id\n1\n2\n3\n4"
    );
}

#[test]
fn an_index_built_on_existing_rows_holds_them_all() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    let report = db.check().unwrap();
    assert_eq!(report.rows, 5);
    assert_eq!(
        rows(&mut db, "SELECT id FROM t WHERE n = 30"),
        "id\n3",
        "built from the rows that were already there"
    );
}

#[test]
fn nulls_are_indexed_and_never_match_a_range() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    db.execute("INSERT INTO t VALUES (6, NULL, 'null')")
        .unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE n < 15"), "id\n1");
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE n IS NULL"), "id\n6");
    db.check().unwrap();
    db.execute("DELETE FROM t WHERE id = 6").unwrap();
    db.check().unwrap();
}

#[test]
fn a_value_too_long_to_index_is_refused_before_anything_is_written() {
    let disks = Disks::new();
    let mut db = disks.open();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, s TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'short')").unwrap();
    db.execute("CREATE INDEX by_s ON t (s)").unwrap();
    let long = "x".repeat(MAX_KEY);

    let insert = format!("INSERT INTO t VALUES (2, 'fine'), (3, '{long}')");
    assert!(err(&mut db, &insert).contains("too long for index by_s"));
    let update = format!("UPDATE t SET s = '{long}'");
    assert!(err(&mut db, &update).contains("too long for index by_s"));
    assert_eq!(rows(&mut db, "SELECT s FROM t"), "s\nshort");
    db.check().unwrap();

    // The same goes for building an index on a row that already holds one.
    db.execute("DROP INDEX by_s").unwrap();
    db.execute(&update).unwrap();
    let pages = db.check().unwrap().pages;
    assert!(err(&mut db, "CREATE INDEX by_s ON t (s)").contains("too long for index by_s"));
    assert!(db.indexes().unwrap().is_empty());
    assert_eq!(db.check().unwrap().pages, pages);
}

#[test]
fn indexes_roll_back() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    let before = db.check().unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("CREATE INDEX by_s ON t (s)").unwrap();
    db.execute("DROP INDEX by_n").unwrap();
    db.execute("INSERT INTO t VALUES (9, 90, 'z')").unwrap();
    assert_eq!(db.indexes().unwrap()[0].name, "by_s");
    db.check().unwrap();
    db.execute("ROLLBACK").unwrap();

    assert_eq!(db.indexes().unwrap()[0].name, "by_n");
    assert_eq!(db.check().unwrap(), before);
    assert_eq!(
        explain(&mut db, "SELECT * FROM t WHERE n = 1")[0],
        "INDEX SCAN by_n ON t (n = 1)"
    );
}

#[test]
fn dropping_a_table_frees_its_indexes() {
    let disks = Disks::new();
    let mut db = disks.open();
    db.execute("CREATE TABLE keep (a INT)").unwrap();
    let before = db.check().unwrap().pages;
    db.execute("CREATE TABLE t (a INT, b INT)").unwrap();
    db.execute("CREATE INDEX i1 ON t (a)").unwrap();
    db.execute("CREATE INDEX i2 ON t (b)").unwrap();
    db.execute("DROP TABLE t").unwrap();

    assert!(db.indexes().unwrap().is_empty());
    let report = db.check().unwrap();
    assert_eq!(report.pages - report.free_pages, before);
}

#[test]
fn indexes_survive_a_restart_and_a_crash() {
    let disks = Disks::new();
    let db = seeded(&disks);
    db.close().unwrap();

    let mut db = disks.open();
    assert_eq!(db.indexes().unwrap().len(), 1);
    db.execute("INSERT INTO t VALUES (6, 60, 'f')").unwrap();
    drop(db);

    let mut db = disks.open();
    db.check().unwrap();
    assert_eq!(rows(&mut db, "SELECT id FROM t WHERE n = 60"), "id\n6");
}

#[test]
fn check_finds_an_entry_missing_from_an_index() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    let tree = index_tree(&mut db, "by_n");
    let (key, _) = tree.iter(&mut db.pager).unwrap().next().unwrap().unwrap();
    tree.delete(&mut db.pager, &key).unwrap();
    let message = db.check().unwrap_err().to_string();
    assert!(message.contains("by_n"), "{message}");
}

#[test]
fn check_finds_an_entry_with_no_row() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    let tree = index_tree(&mut db, "by_n");
    tree.insert(
        &mut db.pager,
        &crate::types::index_key(&[Value::Int(99)], 99),
        &[],
    )
    .unwrap();
    let message = db.check().unwrap_err().to_string();
    assert!(message.contains("by_n"), "{message}");
}

#[test]
fn a_lookup_finds_a_stale_entry_rather_than_returning_nothing() {
    let disks = Disks::new();
    let mut db = seeded(&disks);
    let tree = index_tree(&mut db, "by_n");
    tree.insert(
        &mut db.pager,
        &crate::types::index_key(&[Value::Int(99)], 99),
        &[],
    )
    .unwrap();
    let message = err(&mut db, "SELECT * FROM t WHERE n = 99");
    assert!(message.contains("corrupt"), "{message}");
}

#[test]
fn nan_cannot_be_stored() {
    let disks = Disks::new();
    let mut db = disks.open();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, r REAL)")
        .unwrap();
    let infinity = "1e308 * 10.0";
    db.execute(&format!("INSERT INTO t VALUES (1, {infinity})"))
        .unwrap();
    let nan = format!("{infinity} - {infinity}");
    assert!(err(&mut db, &format!("INSERT INTO t VALUES (2, {nan})")).contains("NaN"));
    assert!(err(&mut db, &format!("UPDATE t SET r = {nan}")).contains("NaN"));
    assert!(rows(&mut db, "SELECT r FROM t").contains("inf"));
}

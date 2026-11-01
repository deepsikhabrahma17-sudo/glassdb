//! Dropped tables, and `VACUUM HISTORY`.

use super::*;

#[test]
fn a_dropped_table_can_still_be_read_in_the_past() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    run(&mut db, "DROP TABLE t"); // 4
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[11, 20]));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 4").contains("no table t at txn 4"));
    assert!(query_error(&mut db, "SELECT * FROM t").contains("no such table"));
    db.check().unwrap();
}

#[test]
fn a_name_can_be_reused_after_a_drop() {
    let (mut db, _) = accounts();
    run(&mut db, "DROP TABLE t"); // 3
    run(&mut db, "CREATE TABLE t (other TEXT)"); // 4
    run(&mut db, "INSERT INTO t VALUES ('new')"); // 5
    run(&mut db, "DROP TABLE t"); // 6
    run(&mut db, "CREATE TABLE t (third INT)"); // 7
    assert_eq!(rows(&mut db, "SELECT * FROM t AS OF TXN 2").len(), 2);
    assert_eq!(
        rows(&mut db, "SELECT other FROM t AS OF TXN 5"),
        [[Value::Text("new".into())]]
    );
    assert!(rows(&mut db, "SELECT third FROM t AS OF TXN 7").is_empty());
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 3").contains("no table t at txn 3"));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 6").contains("no table t at txn 6"));
    db.check().unwrap();
}

#[test]
fn a_drop_rolls_back() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "DROP TABLE t");
    run(&mut db, "ROLLBACK");
    assert_eq!(rows(&mut db, "SELECT n FROM t"), ints(&[10, 20]));
    db.check().unwrap();
}

#[test]
fn a_table_made_and_dropped_in_a_transaction_leaves_nothing() {
    let (mut db, _) = accounts();
    let pages = db.check().unwrap().pages;
    run(&mut db, "BEGIN");
    run(&mut db, "CREATE TABLE gone (a INT)");
    run(&mut db, "INSERT INTO gone VALUES (1)");
    run(&mut db, "DROP TABLE gone");
    run(&mut db, "COMMIT"); // 3
    let report = db.check().unwrap();
    assert_eq!(report.dropped_tables, 0);
    assert_eq!(report.pages - report.free_pages, pages);
}

#[test]
fn drop_frees_indexes_but_keeps_rows_until_vacuum() {
    let (mut db, clock) = accounts();
    run(&mut db, "CREATE INDEX by_n ON t (n)");
    let before = db.check().unwrap();
    run(&mut db, "DROP TABLE t");
    let dropped = db.check().unwrap();
    assert_eq!(dropped.dropped_tables, 1);
    assert_eq!(
        dropped.free_pages,
        before.free_pages + 1,
        "the index's page"
    );

    clock.advance(SECOND);
    let now = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    let vacuumed = db.check().unwrap();
    assert_eq!(vacuumed.dropped_tables, 0);
    assert_eq!(
        vacuumed.free_pages,
        dropped.free_pages + 2,
        "rows and history"
    );
}

#[test]
fn vacuum_removes_versions_older_than_its_time() {
    let (mut db, clock) = accounts();
    clock.advance(10 * SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3, at +10s
    clock.advance(10 * SECOND);
    run(&mut db, "UPDATE t SET n = 12 WHERE id = 1"); // 4, at +20s
    assert_eq!(db.check().unwrap().old_versions, 2);

    let keep_from = format_timestamp(START + 10 * SECOND);
    let output = run(&mut db, &format!("VACUUM HISTORY BEFORE '{keep_from}'"));
    assert!(
        output.to_string().contains("removed 1 old version"),
        "{output}"
    );
    assert_eq!(db.check().unwrap().old_versions, 1);

    // The moment vacuumed to is still exact; the one before it is gone.
    assert_eq!(n_at(&mut db, 3), ints(&[11, 20]));
    assert_eq!(n_at(&mut db, 4), ints(&[12, 20]));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 2").contains("vacuumed"));
    let earlier = format_timestamp(START);
    assert!(
        query_error(&mut db, &format!("SELECT * FROM t AS OF '{earlier}'")).contains("vacuumed")
    );
}

#[test]
fn vacuum_takes_no_id_and_can_be_repeated() {
    let (mut db, clock) = accounts();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    let latest = db.latest_txn().unwrap();
    let now = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    let again = run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    assert!(
        again.to_string().contains("removed 0 old versions"),
        "{again}"
    );
    assert_eq!(db.latest_txn().unwrap(), latest);
    db.check().unwrap();
}

#[test]
fn an_earlier_vacuum_keeps_the_later_floor() {
    let (mut db, clock) = accounts();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    let later = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{later}'"));
    let earlier = format_timestamp(START - SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{earlier}'"));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 2").contains("vacuumed"));
}

#[test]
fn the_floor_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    let mut db = Database::open(&path).unwrap();
    let clock = Arc::new(ManualClock::new(START));
    db.set_clock(clock.clone());
    run(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    clock.advance(SECOND);
    run(&mut db, "INSERT INTO t VALUES (1, 1)");
    clock.advance(SECOND);
    let now = format_timestamp(START + 2 * SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 1").contains("vacuumed"));
    assert_eq!(rows(&mut db, "SELECT * FROM t AS OF TXN 2").len(), 1);
}

#[test]
fn vacuum_waits_for_no_transaction() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    assert!(
        query_error(&mut db, "VACUUM HISTORY BEFORE '2026-01-01'").contains("inside a transaction")
    );
    run(&mut db, "ROLLBACK");
}

#[test]
fn a_vacuumed_row_keeps_its_current_version() {
    let (mut db, clock) = accounts();
    clock.advance(SECOND);
    let now = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    assert_eq!(rows(&mut db, "SELECT n FROM t"), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(rows(&mut db, "SELECT n FROM t HISTORY").len(), 2);
}

//! `HISTORY`, and which versions get stored at all.

use super::*;

#[test]
fn history_lists_every_version_with_times() {
    let (mut db, clock) = accounts();
    clock.advance(SECOND);
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    clock.advance(SECOND);
    run(&mut db, "DELETE FROM t WHERE id = 1"); // 4
    clock.advance(SECOND);
    run(&mut db, "INSERT INTO t VALUES (1, 12)"); // 5

    let Output::Rows { columns, rows } = run(&mut db, "SELECT * FROM t HISTORY WHERE id = 1")
    else {
        panic!()
    };
    assert_eq!(
        columns,
        [
            "id",
            "n",
            "created_txn",
            "created_at",
            "deleted_txn",
            "deleted_at"
        ]
    );
    let time = |s: i64| Value::Text(format_timestamp(START + s * SECOND));
    let row = |n, created: i64, deleted: Option<i64>, t0, t1: Value| {
        vec![
            Value::Int(1),
            Value::Int(n),
            Value::Int(created),
            t0,
            deleted.map_or(Value::Null, Value::Int),
            t1,
        ]
    };
    assert_eq!(
        rows,
        [
            row(10, 2, Some(3), time(0), time(1)),
            row(11, 3, Some(4), time(1), time(2)),
            row(12, 5, None, time(3), Value::Null),
        ]
    );
}

#[test]
fn history_columns_work_in_where_and_order_by() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    run(&mut db, "UPDATE t SET n = 12 WHERE id = 1"); // 4
    assert_eq!(
        rows(
            &mut db,
            "SELECT n FROM t HISTORY WHERE deleted_txn IS NULL ORDER BY n"
        ),
        ints(&[12, 20])
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT n FROM t HISTORY WHERE id = 1 ORDER BY created_txn DESC LIMIT 2"
        ),
        ints(&[12, 11])
    );
}

#[test]
fn a_row_changed_twice_in_a_transaction_has_one_new_version() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    run(&mut db, "UPDATE t SET n = 12 WHERE id = 1");
    run(&mut db, "UPDATE t SET n = 13 WHERE id = 1");
    run(&mut db, "COMMIT"); // 3
    assert_eq!(
        rows(&mut db, "SELECT n FROM t HISTORY WHERE id = 1"),
        ints(&[10, 13])
    );
    assert_eq!(db.check().unwrap().old_versions, 1);
}

#[test]
fn a_row_born_and_gone_in_a_transaction_leaves_nothing() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "INSERT INTO t VALUES (3, 30)");
    run(&mut db, "UPDATE t SET n = 31 WHERE id = 3");
    run(&mut db, "DELETE FROM t WHERE id = 3");
    run(&mut db, "COMMIT");
    assert!(rows(&mut db, "SELECT * FROM t HISTORY WHERE id = 3").is_empty());
    assert_eq!(db.check().unwrap().old_versions, 0);
}

#[test]
fn a_row_deleted_and_reinserted_in_a_transaction_keeps_its_old_version() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "DELETE FROM t WHERE id = 1");
    run(&mut db, "INSERT INTO t VALUES (1, 100)");
    run(&mut db, "COMMIT"); // 3
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert_eq!(n_at(&mut db, 3), ints(&[100, 20]));
    db.check().unwrap();
}

#[test]
fn inside_a_transaction_the_past_stays_committed() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    assert_eq!(n_at(&mut db, 2), ints(&[10, 20]));
    assert!(query_error(&mut db, "SELECT * FROM t AS OF TXN 3").contains("latest is 2"));
    run(&mut db, "COMMIT");
    assert_eq!(n_at(&mut db, 3), ints(&[11, 20]));
}

#[test]
fn history_shows_a_transactions_own_changes_without_times() {
    let (mut db, _) = accounts();
    run(&mut db, "BEGIN");
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1");
    let found = rows(
        &mut db,
        "SELECT n, created_txn, created_at, deleted_at FROM t HISTORY WHERE id = 1",
    );
    assert_eq!(found[1][..3], [Value::Int(11), Value::Int(3), Value::Null]);
    assert_eq!(found[0][3], Value::Null, "txn 3 has no time yet");
    run(&mut db, "ROLLBACK");
}

#[test]
fn history_refuses_a_column_with_its_own_name() {
    let (mut db, _) = open();
    run(&mut db, "CREATE TABLE t (created_at TEXT)");
    assert!(query_error(&mut db, "SELECT * FROM t HISTORY").contains("created_at"));
    run(&mut db, "INSERT INTO t VALUES ('x')");
    assert_eq!(rows(&mut db, "SELECT * FROM t AS OF TXN 2").len(), 1);
}

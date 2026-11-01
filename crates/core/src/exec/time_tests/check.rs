//! `check` on versions, the tracer, and the row size limit.

use super::*;

#[test]
fn check_counts_every_tree() {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 11");
    run(&mut db, "DROP TABLE t");
    run(&mut db, "CREATE TABLE u (a INT)");
    let report = db.check().unwrap();
    assert_eq!(report.tables, 1);
    assert_eq!(report.dropped_tables, 1);
    assert_eq!(report.old_versions, 2);
}

#[test]
fn the_tracer_sees_commits_versions_and_vacuums() {
    let tracer = Arc::new(RecordingTracer::default());
    let mut db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        64,
        tracer.clone(),
    )
    .unwrap();
    let clock = Arc::new(ManualClock::new(START));
    db.set_clock(clock.clone());
    run(&mut db, "CREATE TABLE t (id INT PRIMARY KEY, n INT)");
    run(&mut db, "INSERT INTO t VALUES (1, 10)");
    tracer.take();

    run(&mut db, "UPDATE t SET n = 11");
    let events = tracer.take();
    assert!(events.contains(&Event::VersionKept {
        rowid: 1,
        created: 2,
        deleted: 3
    }));
    assert!(events.contains(&Event::Committed {
        txn: 3,
        time: START
    }));

    rows(&mut db, "SELECT * FROM t AS OF TXN 2");
    assert!(tracer.take().contains(&Event::PastRead {
        txn: 2,
        visible: 1,
        hidden: 1
    }));

    clock.advance(SECOND);
    let now = format_timestamp(START + SECOND);
    run(&mut db, &format!("VACUUM HISTORY BEFORE '{now}'"));
    assert!(tracer.take().contains(&Event::Vacuumed {
        txn: 3,
        versions: 1,
        tables: 0
    }));
}

/// `t`'s table and history trees, for changing them behind the engine's back.
fn trees(db: &mut Database) -> (BTree, BTree) {
    let table = db.tables().unwrap().remove(0);
    (BTree::open(table.root), BTree::open(table.history_root))
}

fn corrupt(db: &mut Database) -> String {
    match db.check() {
        Err(Error::Corrupt(msg)) => msg,
        other => panic!("{other:?}"),
    }
}

/// `accounts` with one old version, and its key and row bytes.
fn with_old_version() -> (Database, Vec<u8>, Vec<u8>) {
    let (mut db, _) = accounts();
    run(&mut db, "UPDATE t SET n = 11 WHERE id = 1"); // 3
    let (_, history) = trees(&mut db);
    let (key, value) = db.with_pager(|pager| history.iter(pager).unwrap().next().unwrap().unwrap());
    let row = unstamped(&value).unwrap().1.to_vec();
    (db, key, row)
}

#[test]
fn check_finds_a_row_from_the_future() {
    let (mut db, _) = accounts();
    let (table, _) = trees(&mut db);
    let (key, value) = db.with_pager(|pager| table.iter(pager).unwrap().next().unwrap().unwrap());
    let row = unstamped(&value).unwrap().1.to_vec();
    db.with_pager(|pager| table.insert(pager, &key, &stamped(9, &row)).unwrap());
    assert!(corrupt(&mut db).contains("hasn't happened"));
}

#[test]
fn check_finds_a_version_deleted_by_the_future() {
    let (mut db, key, row) = with_old_version();
    let (_, history) = trees(&mut db);
    db.with_pager(|pager| history.insert(pager, &key, &stamped(9, &row)).unwrap());
    assert!(corrupt(&mut db).contains("hasn't happened"));
}

#[test]
fn check_finds_a_version_deleted_before_it_began() {
    let (mut db, key, row) = with_old_version();
    let (_, history) = trees(&mut db);
    db.with_pager(|pager| history.insert(pager, &key, &stamped(2, &row)).unwrap());
    assert!(corrupt(&mut db).contains("before it was created"));
}

#[test]
fn check_finds_overlapping_versions() {
    let (mut db, key, row) = with_old_version();
    let (_, history) = trees(&mut db);
    // Row 1's version from txn 2 now runs past the start of its replacement.
    db.with_pager(|pager| {
        let (id, _) = split_history_key(&key).unwrap();
        history
            .insert(pager, &history_key(id, 1), &stamped(3, &row))
            .unwrap()
    });
    assert!(corrupt(&mut db).contains("overlaps"));
}

#[test]
fn check_finds_a_version_older_than_the_current_rows_table() {
    let (mut db, key, row) = with_old_version();
    let (_, history) = trees(&mut db);
    let (id, _) = split_history_key(&key).unwrap();
    db.with_pager(|pager| {
        history
            .insert(pager, &history_key(id, 0), &stamped(2, &row))
            .unwrap()
    });
    assert!(corrupt(&mut db).contains("older than its table"));
}

#[test]
fn check_finds_a_version_vacuum_should_have_removed() {
    let (mut db, key, row) = with_old_version();
    let (_, history) = trees(&mut db);
    db.with_pager(|pager| pager.set_history_floor(3));
    assert!(corrupt(&mut db).contains("vacuumed"));
    db.with_pager(|pager| {
        pager.set_history_floor(0);
        history.delete(pager, &key).unwrap();
        history.insert(pager, &key, &stamped(3, &row)).unwrap();
    });
    db.check().unwrap();
}

#[test]
fn check_finds_a_lost_commit() {
    let (mut db, _) = accounts();
    db.with_pager(|pager| {
        BTree::open(COMMITS_ROOT)
            .delete(pager, &2u64.to_be_bytes())
            .unwrap()
    });
    assert!(corrupt(&mut db).contains("commit log"));
}

#[test]
fn the_largest_row_still_fits_in_the_history_tree() {
    let (mut db, _) = open();
    run(&mut db, "CREATE TABLE t (s TEXT)");
    // A row with one TEXT value spends 8 bytes on its own header. A row's
    // stored size is what the limit counts, so step up to it.
    let text = |len: usize| "x".repeat(len);
    let mut fits = 0;
    for len in (900..1000).rev() {
        if db
            .execute(&format!("INSERT INTO t VALUES ('{}')", text(len)))
            .is_ok()
        {
            fits = len;
            break;
        }
    }
    assert!(fits > 900);
    run(&mut db, "UPDATE t SET s = 'small'");
    assert_eq!(db.check().unwrap().old_versions, 1);
    let at = db.latest_txn().unwrap() - 1;
    assert_eq!(
        rows(&mut db, &format!("SELECT s FROM t AS OF TXN {at}")),
        [[Value::Text(text(fits))]]
    );
}

#[test]
fn check_finds_a_floor_ahead_of_the_last_commit() {
    let (mut db, _) = accounts();
    db.with_pager(|pager| pager.set_history_floor(3));
    assert!(corrupt(&mut db).contains("floor"));
}

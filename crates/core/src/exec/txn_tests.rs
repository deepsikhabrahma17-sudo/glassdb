//! Transactions through the `Database` API.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::storage::MemStorage;
use crate::trace::{Event, RecordingTracer};

/// A disk whose log can be made to fail on demand.
struct FlakyLog {
    inner: MemStorage,
    failing: Arc<AtomicBool>,
}

impl Storage for FlakyLog {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(Error::Io(io::Error::other("disk full")));
        }
        self.inner.write_at(offset, data)
    }
    fn truncate(&mut self, len: u64) -> Result<()> {
        self.inner.truncate(len)
    }
    fn size(&mut self) -> Result<u64> {
        self.inner.size()
    }
    fn sync(&mut self) -> Result<()> {
        self.inner.sync()
    }
}

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

    /// Open with a small cache, so big transactions spill to the log.
    fn open(&self) -> Database {
        Database::open_with(
            Box::new(self.db.clone()),
            Box::new(self.log.clone()),
            4,
            self.tracer.clone(),
        )
        .unwrap()
    }
}

/// A database holding `t` with rows (1, 'one') and (2, 'two').
fn with_table(disks: &Disks) -> Database {
    let mut db = disks.open();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    db
}

fn ids(db: &mut Database) -> Vec<i64> {
    match db.execute("SELECT id FROM t ORDER BY id").unwrap() {
        Output::Rows { rows, .. } => rows
            .iter()
            .map(|row| match row[0] {
                Value::Int(n) => n,
                _ => panic!("bad id"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

/// Insert enough rows to overflow a 4-frame cache.
fn insert_many(db: &mut Database) {
    let name = "x".repeat(300);
    for id in 100..160 {
        db.execute(&format!("INSERT INTO t VALUES ({id}, '{name}')"))
            .unwrap();
    }
}

fn err(db: &mut Database, sql: &str) -> String {
    db.execute(sql).unwrap_err().to_string()
}

#[test]
fn commit_keeps_the_changes() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    assert_eq!(
        db.execute("BEGIN").unwrap(),
        Output::Message("transaction started".into())
    );
    assert!(db.in_transaction());
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(
        db.execute("COMMIT").unwrap(),
        Output::Message("committed".into())
    );
    assert!(!db.in_transaction());
    drop(db); // a crash

    assert_eq!(ids(&mut disks.open()), vec![2, 3]);
}

#[test]
fn rollback_undoes_every_kind_of_change() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    db.execute("UPDATE t SET name = 'changed'").unwrap();
    db.execute("DELETE FROM t WHERE id = 1").unwrap();
    assert_eq!(
        ids(&mut db),
        vec![2, 3],
        "a transaction sees its own changes"
    );
    assert_eq!(
        db.execute("ROLLBACK").unwrap(),
        Output::Message("rolled back".into())
    );
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), vec![1, 2]);
    assert_eq!(
        db.execute("SELECT name FROM t WHERE id = 1")
            .unwrap()
            .to_string(),
        "name\none"
    );
    db.check().unwrap();
}

#[test]
fn rollback_undoes_a_transaction_bigger_than_the_cache() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    let before = db.check().unwrap();
    db.execute("BEGIN").unwrap();
    insert_many(&mut db);
    db.execute("ROLLBACK").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2]);
    assert_eq!(db.check().unwrap(), before);
}

#[test]
fn table_changes_roll_back_too() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    let before = db.check().unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("CREATE TABLE ghost (a INT)").unwrap();
    db.execute("INSERT INTO ghost VALUES (1)").unwrap();
    db.execute("DROP TABLE t").unwrap();
    assert_eq!(db.tables().unwrap()[0].name, "ghost");
    db.execute("ROLLBACK").unwrap();

    let tables = db.tables().unwrap();
    assert_eq!(tables.len(), 1);
    assert_eq!(tables[0].name, "t");
    assert_eq!(ids(&mut db), vec![1, 2]);
    assert_eq!(db.check().unwrap(), before);
}

#[test]
fn a_crash_inside_a_transaction_loses_only_that_transaction() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    db.execute("BEGIN").unwrap();
    insert_many(&mut db);
    drop(db);

    let mut db = disks.open();
    assert_eq!(ids(&mut db), vec![1, 2]);
    db.check().unwrap();
}

#[test]
fn a_rolled_back_transaction_leaves_no_trace_after_a_crash() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    let log = disks.log.to_bytes();
    db.execute("BEGIN").unwrap();
    insert_many(&mut db);
    assert!(disks.log.to_bytes().len() > log.len(), "the log must spill");
    db.execute("ROLLBACK").unwrap();
    assert_eq!(disks.log.to_bytes(), log, "the log is back as it was");
    drop(db);

    let mut db = disks.open();
    assert_eq!(ids(&mut db), vec![1, 2]);
    db.check().unwrap();
}

#[test]
fn a_transaction_is_one_commit() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    disks.tracer.take();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'a')").unwrap();
    db.execute("INSERT INTO t VALUES (4, 'b')").unwrap();
    let inside = disks.tracer.take();
    assert!(inside.contains(&Event::TxnBegin));
    assert!(
        !inside.iter().any(|e| matches!(e, Event::WalCommit { .. })),
        "nothing commits before COMMIT"
    );
    db.execute("COMMIT").unwrap();
    let events = disks.tracer.take();
    let commits = |e: &&Event| matches!(e, Event::WalCommit { .. });
    assert_eq!(events.iter().filter(commits).count(), 1);
    assert!(events.contains(&Event::TxnCommit));
}

#[test]
fn an_empty_transaction_writes_nothing() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    let log = disks.log.to_bytes();
    db.execute("BEGIN").unwrap();
    db.execute("COMMIT").unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("SELECT * FROM t").unwrap();
    db.execute("ROLLBACK").unwrap();
    assert_eq!(disks.log.to_bytes(), log);
}

#[test]
fn transaction_statements_must_make_sense() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    assert!(err(&mut db, "COMMIT").contains("no transaction"));
    assert!(err(&mut db, "ROLLBACK").contains("no transaction"));
    db.execute("BEGIN").unwrap();
    assert!(err(&mut db, "BEGIN").contains("already in a transaction"));
    assert!(db.in_transaction(), "the first BEGIN still stands");
    db.execute("ROLLBACK").unwrap();
}

#[test]
fn a_rejected_statement_keeps_the_transaction_open() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    assert!(err(&mut db, "INSERT INTO t VALUES (3, 'again')").contains("duplicate"));
    assert!(err(&mut db, "SELECT * FROM missing").contains("no such table"));
    assert!(db.in_transaction());
    db.execute("INSERT INTO t VALUES (4, 'four')").unwrap();
    db.execute("COMMIT").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2, 3, 4]);
}

#[test]
fn a_checkpoint_waits_for_the_transaction() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    let message = db.checkpoint().unwrap_err().to_string();
    assert!(message.contains("inside a transaction"), "{message}");
    db.execute("ROLLBACK").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2]);
    db.checkpoint().unwrap();
}

#[test]
fn close_rolls_back_an_open_transaction() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    db.execute("BEGIN").unwrap();
    insert_many(&mut db);
    db.close().unwrap();
    assert!(disks.log.to_bytes().is_empty());
    assert_eq!(ids(&mut disks.open()), vec![1, 2]);
}

#[test]
fn a_big_rollback_does_not_count_towards_a_checkpoint() {
    let disks = Disks::new();
    let mut db = with_table(&disks);
    for _ in 0..3 {
        db.execute("BEGIN").unwrap();
        insert_many(&mut db);
        db.execute("ROLLBACK").unwrap();
    }
    disks.tracer.take();
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    assert!(
        !disks
            .tracer
            .take()
            .iter()
            .any(|e| matches!(e, Event::Checkpoint { .. }))
    );
}

/// A database on a log that can fail, with a table already committed.
fn flaky() -> (Database, Arc<AtomicBool>, MemStorage, MemStorage) {
    let (main, log) = (MemStorage::new(), MemStorage::new());
    let failing = Arc::new(AtomicBool::new(false));
    let mut db = Database::open_with(
        Box::new(main.clone()),
        Box::new(FlakyLog {
            inner: log.clone(),
            failing: failing.clone(),
        }),
        4,
        Arc::new(NoopTracer),
    )
    .unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 'one'), (2, 'two')")
        .unwrap();
    (db, failing, main, log)
}

#[test]
fn a_disk_error_fails_the_transaction_until_rollback() {
    let (mut db, failing, ..) = flaky();
    db.execute("BEGIN").unwrap();
    failing.store(true, Ordering::SeqCst);
    let name = "x".repeat(300);
    let rows: Vec<String> = (10..60).map(|id| format!("({id}, '{name}')")).collect();
    let insert = format!("INSERT INTO t VALUES {}", rows.join(", "));
    assert!(err(&mut db, &insert).contains("disk full"));
    failing.store(false, Ordering::SeqCst);

    // Half a statement may be in there, so nothing else can run.
    assert!(db.in_transaction());
    for sql in ["SELECT * FROM t", "INSERT INTO t VALUES (3, 'x')", "COMMIT"] {
        assert!(err(&mut db, sql).contains("ROLLBACK"), "{sql}");
    }
    assert!(err(&mut db, "BEGIN").contains("already in a transaction"));

    db.execute("ROLLBACK").unwrap();
    assert!(!db.in_transaction());
    assert_eq!(ids(&mut db), vec![1, 2]);
    db.check().unwrap();
}

#[test]
fn a_failed_commit_can_be_rolled_back() {
    let (mut db, failing, ..) = flaky();
    db.execute("BEGIN").unwrap();
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    failing.store(true, Ordering::SeqCst);
    assert!(err(&mut db, "COMMIT").contains("disk full"));
    failing.store(false, Ordering::SeqCst);
    assert!(err(&mut db, "COMMIT").contains("ROLLBACK"));
    db.execute("ROLLBACK").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2]);
}

#[test]
fn a_statement_that_fails_part_way_is_undone() {
    // Outside a transaction, a half-written statement must not be swept up
    // by the next statement's commit.
    let (mut db, failing, ..) = flaky();
    let name = "x".repeat(300);
    let rows: Vec<String> = (10..60).map(|id| format!("({id}, '{name}')")).collect();
    let insert = format!("INSERT INTO t VALUES {}", rows.join(", "));
    failing.store(true, Ordering::SeqCst);
    assert!(err(&mut db, &insert).contains("disk full"));
    failing.store(false, Ordering::SeqCst);

    assert!(!db.in_transaction());
    db.execute("INSERT INTO t VALUES (3, 'three')").unwrap();
    assert_eq!(ids(&mut db), vec![1, 2, 3]);
    db.check().unwrap();
}

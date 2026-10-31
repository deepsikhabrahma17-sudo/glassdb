//! Random transactions on a tiny cache, compared with a simple in-memory model.
//!
//! The model holds the committed state and, during a transaction, a copy to
//! work on: COMMIT keeps the copy and ROLLBACK or a crash throws it away. The
//! database must agree after every step, and pass `check` at every boundary.

use std::collections::BTreeMap;
use std::sync::Arc;

use glassdb_core::{Database, MemStorage, NoopTracer, Output, Value};
use proptest::prelude::*;

/// Small enough that a few inserts push uncommitted pages out to the log.
const FRAMES: usize = 4;

#[derive(Debug, Clone)]
enum Op {
    Insert(i64, usize),
    Update(i64, usize),
    Delete(i64),
    Begin,
    Commit,
    Rollback,
    CreateExtra,
    DropExtra,
    Crash,
    Restart,
}

fn op() -> impl Strategy<Value = Op> {
    let key = 0i64..120;
    let len = 1usize..600;
    prop_oneof![
        8 => (key.clone(), len.clone()).prop_map(|(k, n)| Op::Insert(k, n)),
        3 => (key.clone(), len).prop_map(|(k, n)| Op::Update(k, n)),
        3 => key.prop_map(Op::Delete),
        3 => Just(Op::Begin),
        2 => Just(Op::Commit),
        2 => Just(Op::Rollback),
        1 => Just(Op::CreateExtra),
        1 => Just(Op::DropExtra),
        1 => Just(Op::Crash),
        1 => Just(Op::Restart),
    ]
}

#[derive(Debug, Clone, Default, PartialEq)]
struct State {
    rows: BTreeMap<i64, usize>,
    extra: bool,
}

#[derive(Default)]
struct Model {
    committed: State,
    /// The running transaction's working copy.
    open: Option<State>,
}

impl Model {
    fn view(&mut self) -> &mut State {
        self.open.as_mut().unwrap_or(&mut self.committed)
    }
}

struct Disks {
    db: MemStorage,
    log: MemStorage,
}

impl Disks {
    fn open(&self) -> Database {
        Database::open_with(
            Box::new(self.db.clone()),
            Box::new(self.log.clone()),
            FRAMES,
            Arc::new(NoopTracer),
        )
        .unwrap()
    }
}

/// What the database actually holds, in the model's terms.
fn observe(db: &mut Database) -> State {
    let rows = match db.execute("SELECT id, pad FROM t").unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    };
    let mut state = State::default();
    for row in rows {
        match (&row[0], &row[1]) {
            (Value::Int(id), Value::Text(pad)) => {
                assert!(pad.bytes().all(|b| b == b'x'), "a corrupted row");
                state.rows.insert(*id, pad.len());
            }
            other => panic!("bad row {other:?}"),
        }
    }
    state.extra = db.tables().unwrap().iter().any(|t| t.name == "extra");
    state
}

fn apply(op: &Op, db: &mut Option<Database>, disks: &Disks, model: &mut Model) {
    let conn = db.as_mut().unwrap();
    let pad = |n: usize| "x".repeat(n);
    match op {
        Op::Insert(id, n) => {
            let result = conn.execute(&format!("INSERT INTO t VALUES ({id}, '{}')", pad(*n)));
            let free = !model.view().rows.contains_key(id);
            assert_eq!(result.is_ok(), free, "insert {id}");
            if free {
                model.view().rows.insert(*id, *n);
            }
        }
        Op::Update(id, n) => {
            let sql = format!("UPDATE t SET pad = '{}' WHERE id = {id}", pad(*n));
            conn.execute(&sql).unwrap();
            if let Some(row) = model.view().rows.get_mut(id) {
                *row = *n;
            }
        }
        Op::Delete(id) => {
            conn.execute(&format!("DELETE FROM t WHERE id = {id}"))
                .unwrap();
            model.view().rows.remove(id);
        }
        Op::Begin => {
            let result = conn.execute("BEGIN");
            assert_eq!(result.is_ok(), model.open.is_none());
            if model.open.is_none() {
                model.open = Some(model.committed.clone());
            }
        }
        Op::Commit => {
            assert_eq!(conn.execute("COMMIT").is_ok(), model.open.is_some());
            if let Some(state) = model.open.take() {
                model.committed = state;
            }
        }
        Op::Rollback => {
            assert_eq!(conn.execute("ROLLBACK").is_ok(), model.open.is_some());
            model.open = None;
        }
        Op::CreateExtra => {
            let exists = model.view().extra;
            let result = conn.execute("CREATE TABLE extra (a INT)");
            assert_eq!(result.is_ok(), !exists);
            model.view().extra = true;
        }
        Op::DropExtra => {
            let exists = model.view().extra;
            assert_eq!(conn.execute("DROP TABLE extra").is_ok(), exists);
            model.view().extra = false;
        }
        Op::Crash => {
            *db = None; // dropped without a close, as a crash would
            *db = Some(disks.open());
            model.open = None;
        }
        Op::Restart => {
            db.take().unwrap().close().unwrap();
            *db = Some(disks.open());
            model.open = None;
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn transactions_match_the_model(ops in prop::collection::vec(op(), 1..120)) {
        let disks = Disks { db: MemStorage::new(), log: MemStorage::new() };
        let mut db = Some(disks.open());
        db.as_mut().unwrap()
            .execute("CREATE TABLE t (id INT PRIMARY KEY, pad TEXT)")
            .unwrap();
        let mut model = Model::default();

        for op in &ops {
            apply(op, &mut db, &disks, &mut model);
            let conn = db.as_mut().unwrap();
            prop_assert_eq!(conn.in_transaction(), model.open.is_some());
            prop_assert_eq!(&observe(conn), model.view());
            if model.open.is_none() {
                conn.check().unwrap();
            }
        }

        // However it ended, a crash now leaves exactly what was committed.
        db = None;
        drop(db);
        let mut conn = disks.open();
        prop_assert_eq!(observe(&mut conn), model.committed);
        conn.check().unwrap();
    }
}

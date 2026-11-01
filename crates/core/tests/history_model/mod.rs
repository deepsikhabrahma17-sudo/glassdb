//! The model `history_props` compares the engine with. See that file.

use std::collections::BTreeMap;
use std::sync::Arc;

use glassdb_core::time::format_timestamp;
use glassdb_core::{Clock, Database, Error, ManualClock, Output, Reader, Value};

mod check;

const START: i64 = 1_793_534_400_000_000; // 2026-11-01 12:00:00 UTC
const SECOND: i64 = 1_000_000;
pub const NAMES: [&str; 2] = ["a", "b"];

type Row = (i64, i64);
/// Each table that exists, with its rows by id.
type State = BTreeMap<String, BTreeMap<i64, i64>>;

#[derive(Debug, Clone)]
pub enum Op {
    Insert {
        table: usize,
        id: i64,
        n: i64,
    },
    Update {
        table: usize,
        low: i64,
        span: i64,
        n: i64,
    },
    Delete {
        table: usize,
        low: i64,
        span: i64,
    },
    MoveKey {
        table: usize,
        id: i64,
    },
    Begin,
    Commit,
    Rollback,
    CreateIndex {
        table: usize,
    },
    Create {
        table: usize,
    },
    Drop {
        table: usize,
    },
    Vacuum {
        age: u64,
    },
    Pin,
    Unpin,
    Reopen,
    Checkpoint,
}

fn rows_of(output: Output) -> Vec<Vec<Value>> {
    match output {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn pairs(rows: &[Vec<Value>]) -> Vec<Row> {
    rows.iter()
        .map(|row| match (&row[0], &row[1]) {
            (Value::Int(id), Value::Int(n)) => (*id, *n),
            other => panic!("bad row {other:?}"),
        })
        .collect()
}

fn model_rows(state: &State, table: &str) -> Option<Vec<Row>> {
    let rows = state.get(table)?;
    Some(rows.iter().map(|(id, n)| (*id, *n)).collect())
}

/// The engine, the model and everything saved so far.
pub struct Run {
    path: std::path::PathBuf,
    db: Option<Database>,
    clock: Arc<ManualClock>,
    /// The state being changed. Inside a transaction, it may yet be undone.
    state: State,
    /// The state as of the last commit.
    committed: State,
    in_transaction: bool,
    /// Whether anything changed in this transaction or statement.
    changed: bool,
    /// The state at each transaction, and its commit time. `[0]` is the start.
    past: Vec<(State, i64)>,
    readers: Vec<(Reader, State)>,
    floor: u64,
    indexes: usize,
    /// The transaction that made each table, and the same as of the last commit.
    born: BTreeMap<String, u64>,
    committed_born: BTreeMap<String, u64>,
}

impl Run {
    pub fn new(path: std::path::PathBuf) -> Run {
        let clock = Arc::new(ManualClock::new(START));
        let mut db = Database::open(&path).unwrap();
        db.set_clock(clock.clone());
        let mut run = Run {
            path,
            db: Some(db),
            clock,
            state: State::new(),
            committed: State::new(),
            in_transaction: false,
            changed: false,
            past: vec![(State::new(), i64::MIN)],
            readers: Vec::new(),
            floor: 0,
            indexes: 0,
            born: BTreeMap::new(),
            committed_born: BTreeMap::new(),
        };
        run.create("a");
        run.create("b");
        run
    }

    fn db(&mut self) -> &mut Database {
        self.db.as_mut().unwrap()
    }

    fn sql(&mut self, sql: &str) {
        self.db()
            .execute(sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }

    /// A statement that took effect: it commits now unless a transaction is open.
    fn wrote(&mut self) {
        self.changed = true;
        if !self.in_transaction {
            self.committed();
        }
    }

    fn committed(&mut self) {
        self.committed = self.state.clone();
        self.committed_born = self.born.clone();
        if self.changed {
            self.changed = false;
            let time = self.clock.now();
            self.past.push((self.state.clone(), time));
            let latest = self.past.len() as u64 - 1;
            assert_eq!(self.db().latest_txn().unwrap(), latest);
            self.clock.advance(SECOND);
        }
    }

    fn create(&mut self, table: &str) {
        self.sql(&format!("CREATE TABLE {table} (id INT PRIMARY KEY, n INT)"));
        self.state.insert(table.to_string(), BTreeMap::new());
        self.born.insert(table.to_string(), self.past.len() as u64);
        self.wrote();
    }

    pub fn apply(&mut self, op: &Op) {
        match *op {
            Op::Insert { table, id, n } => {
                let name = NAMES[table];
                let Some(rows) = self.state.get_mut(name) else {
                    return;
                };
                if rows.contains_key(&id) {
                    let sql = format!("INSERT INTO {name} VALUES ({id}, {n})");
                    assert!(matches!(self.db().execute(&sql), Err(Error::Query(_))));
                    return;
                }
                rows.insert(id, n);
                self.sql(&format!("INSERT INTO {name} VALUES ({id}, {n})"));
                self.wrote();
            }
            Op::Update {
                table,
                low,
                span,
                n,
            } => {
                let name = NAMES[table];
                let Some(rows) = self.state.get_mut(name) else {
                    return;
                };
                let matched: Vec<i64> = rows.range(low..=low + span).map(|(id, _)| *id).collect();
                for id in &matched {
                    rows.insert(*id, n);
                }
                self.sql(&format!(
                    "UPDATE {name} SET n = {n} WHERE id >= {low} AND id <= {}",
                    low + span
                ));
                if !matched.is_empty() {
                    self.wrote();
                }
            }
            Op::Delete { table, low, span } => {
                let name = NAMES[table];
                let Some(rows) = self.state.get_mut(name) else {
                    return;
                };
                let matched: Vec<i64> = rows.range(low..=low + span).map(|(id, _)| *id).collect();
                for id in &matched {
                    rows.remove(id);
                }
                self.sql(&format!(
                    "DELETE FROM {name} WHERE id >= {low} AND id <= {}",
                    low + span
                ));
                if !matched.is_empty() {
                    self.wrote();
                }
            }
            Op::MoveKey { table, id } => {
                let name = NAMES[table];
                let target = 100 + id;
                let Some(rows) = self.state.get_mut(name) else {
                    return;
                };
                if rows.contains_key(&target) {
                    return;
                }
                let Some(n) = rows.remove(&id) else { return };
                rows.insert(target, n);
                self.sql(&format!("UPDATE {name} SET id = {target} WHERE id = {id}"));
                self.wrote();
            }
            Op::Begin => {
                if !self.in_transaction {
                    self.sql("BEGIN");
                    self.in_transaction = true;
                }
            }
            Op::Commit => {
                if self.in_transaction {
                    self.sql("COMMIT");
                    self.in_transaction = false;
                    self.committed();
                }
            }
            Op::Rollback => {
                if self.in_transaction {
                    self.sql("ROLLBACK");
                    self.in_transaction = false;
                    self.changed = false;
                    self.state = self.committed.clone();
                    self.born = self.committed_born.clone();
                }
            }
            Op::CreateIndex { table } => {
                let name = NAMES[table];
                if self.state.contains_key(name) {
                    self.indexes += 1;
                    let index = self.indexes;
                    self.sql(&format!("CREATE INDEX i{index} ON {name} (n)"));
                    self.wrote();
                }
            }
            Op::Create { table } => {
                if !self.state.contains_key(NAMES[table]) {
                    self.create(NAMES[table]);
                }
            }
            Op::Drop { table } => {
                let name = NAMES[table];
                if self.state.remove(name).is_some() {
                    self.sql(&format!("DROP TABLE {name}"));
                    self.wrote();
                }
            }
            Op::Vacuum { age } => self.vacuum(age),
            Op::Pin => {
                let reader = self.db().reader().unwrap();
                let state = self.past[reader.txn() as usize].0.clone();
                self.readers.push((reader, state));
            }
            Op::Unpin => {
                self.readers.pop();
            }
            Op::Reopen => {
                // Readers die with their database, and a close ends a transaction.
                if !self.in_transaction {
                    self.readers.clear();
                    self.db.take().unwrap().close().unwrap();
                    let mut db = Database::open(&self.path).unwrap();
                    db.set_clock(self.clock.clone());
                    self.db = Some(db);
                }
            }
            Op::Checkpoint => {
                if !self.in_transaction {
                    self.db().checkpoint().unwrap();
                }
            }
        }
    }

    /// Vacuum away everything `age` commits older than the latest.
    fn vacuum(&mut self, age: u64) {
        if self.in_transaction {
            return;
        }
        let latest = self.past.len() as u64 - 1;
        let Some(target) = latest.checked_sub(age) else {
            return;
        };
        if target == 0 {
            return;
        }
        let time = format_timestamp(self.past[target as usize].1);
        let sql = format!("VACUUM HISTORY BEFORE '{time}'");
        let held = self.readers.iter().any(|(reader, _)| reader.txn() < target);
        match self.db().execute(&sql) {
            Ok(_) => {
                assert!(!held, "vacuum passed a reader");
                self.floor = self.floor.max(target);
            }
            Err(Error::Query(msg)) => assert!(held && msg.contains("reader"), "{msg}"),
            Err(err) => panic!("{err}"),
        }
    }

    pub fn finish(mut self) {
        if self.in_transaction {
            self.apply(&Op::Rollback);
        }
        self.check_readers();
        self.check_past();
        // And once more from the file.
        self.apply(&Op::Reopen);
        self.check_past();
        self.db.take().unwrap().close().unwrap();
    }
}

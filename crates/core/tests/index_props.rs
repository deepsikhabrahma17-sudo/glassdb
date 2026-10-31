//! An index changes speed, never results.
//!
//! The same random statements run on two databases, one with indexes and one
//! without. Every statement must give exactly the same answer on both, rows
//! in the same order, through commits, rollbacks and crashes. `check` also
//! proves that each index holds exactly its table's rows.

use std::sync::Arc;

use glassdb_core::{Database, MemStorage, NoopTracer};
use proptest::prelude::*;

/// Small, so index scans and row fetches spill and evict like a big table.
const FRAMES: usize = 8;
const COLUMNS: &str = "id INT PRIMARY KEY, a INT, b TEXT, c REAL, d BOOL";
const INDEXES: [(&str, &str); 6] = [
    ("by_a", "a"),
    ("by_b", "b"),
    ("by_c", "c"),
    ("by_a_b", "a, b"),
    ("by_d_a_c", "d, a, c"),
    ("by_b_id", "b, id"),
];

#[derive(Debug, Clone)]
enum Op {
    Insert(String),
    Write(String),
    Select(String),
    CreateIndex(usize),
    DropIndex(usize),
    Txn(&'static str),
    Crash,
}

fn int() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => (-3i64..8).prop_map(|n| n.to_string()),
        1 => Just("NULL".to_string()),
    ]
}

fn text() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => prop::sample::select(vec!["", "a", "ab", "abc", "b", "a\u{0}b", "é"])
            .prop_map(|s| format!("'{s}'")),
        1 => Just("NULL".to_string()),
    ]
}

fn real() -> impl Strategy<Value = String> {
    prop_oneof![
        5 => prop::sample::select(vec!["-1.5", "0.0", "-0.0", "0.5", "1.0", "2.5", "1e308 * 10.0"])
            .prop_map(str::to_string),
        1 => Just("NULL".to_string()),
    ]
}

fn boolean() -> impl Strategy<Value = String> {
    prop::sample::select(vec!["TRUE", "FALSE", "NULL"]).prop_map(str::to_string)
}

fn cmp() -> impl Strategy<Value = &'static str> {
    prop::sample::select(vec!["=", "=", "<", "<=", ">", ">=", "<>"])
}

/// One condition. The types always fit, because a wrong type is an error that
/// a scan raises on every row but an index only on the rows it finds.
fn condition() -> impl Strategy<Value = String> {
    let number = prop_oneof![int(), real()];
    prop_oneof![
        (cmp(), int()).prop_map(|(op, v)| format!("a {op} {v}")),
        (cmp(), int()).prop_map(|(op, v)| format!("{v} {op} a")),
        (cmp(), int()).prop_map(|(op, v)| format!("id {op} {v}")),
        (cmp(), real()).prop_map(|(op, v)| format!("a {op} {v}")),
        (cmp(), text()).prop_map(|(op, v)| format!("b {op} {v}")),
        (cmp(), number).prop_map(|(op, v)| format!("c {op} {v}")),
        (cmp(), boolean()).prop_map(|(op, v)| format!("d {op} {v}")),
        (cmp(), -3i64..8).prop_map(|(op, n)| format!("a {op} {n} + 1")),
        Just("a IS NULL".to_string()),
        Just("b IS NOT NULL".to_string()),
        Just("a = id".to_string()),
    ]
}

fn filter() -> impl Strategy<Value = String> {
    let some = || prop::collection::vec(condition(), 1..4);
    (some(), some(), any::<bool>()).prop_map(|(all, others, or)| {
        let all = all.join(" AND ");
        if or {
            format!("{all} OR {}", others.join(" AND "))
        } else {
            all
        }
    })
}

fn where_clause() -> impl Strategy<Value = String> {
    prop_oneof![
        1 => Just(String::new()),
        8 => filter().prop_map(|f| format!(" WHERE {f}")),
    ]
}

fn insert() -> impl Strategy<Value = String> {
    (-30i64..30, int(), text(), real(), boolean())
        .prop_map(|(id, a, b, c, d)| format!("({id}, {a}, {b}, {c}, {d})"))
}

fn op() -> impl Strategy<Value = Op> {
    let update = prop_oneof![
        int().prop_map(|v| format!("a = {v}")),
        Just("a = a + 1".to_string()),
        text().prop_map(|v| format!("b = {v}")),
        real().prop_map(|v| format!("c = {v}")),
        boolean().prop_map(|v| format!("d = {v}")),
        Just("id = id + 100".to_string()),
    ];
    prop_oneof![
        8 => prop::collection::vec(insert(), 1..5)
            .prop_map(|rows| Op::Insert(format!("INSERT INTO t VALUES {}", rows.join(", ")))),
        3 => (update, where_clause()).prop_map(|(set, w)| Op::Write(format!("UPDATE t SET {set}{w}"))),
        2 => where_clause().prop_map(|w| Op::Write(format!("DELETE FROM t{w}"))),
        10 => (where_clause(), any::<bool>()).prop_map(|(w, limit)| {
            let tail = if limit { " ORDER BY a DESC LIMIT 3" } else { "" };
            Op::Select(format!("SELECT * FROM t{w}{tail}"))
        }),
        1 => (0..INDEXES.len()).prop_map(Op::CreateIndex),
        1 => (0..INDEXES.len()).prop_map(Op::DropIndex),
        1 => prop::sample::select(vec!["BEGIN", "COMMIT", "ROLLBACK"]).prop_map(Op::Txn),
        1 => Just(Op::Crash),
    ]
}

struct Disks {
    db: MemStorage,
    log: MemStorage,
}

impl Disks {
    fn new() -> Disks {
        Disks {
            db: MemStorage::new(),
            log: MemStorage::new(),
        }
    }

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

/// A statement's outcome as text, errors included.
fn run(db: &mut Database, sql: &str) -> String {
    match db.execute(sql) {
        Ok(output) => output.to_string(),
        Err(err) => format!("error: {err}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn an_index_never_changes_the_answer(ops in prop::collection::vec(op(), 1..150)) {
        let (with_disks, plain_disks) = (Disks::new(), Disks::new());
        let (mut with, mut plain) = (with_disks.open(), plain_disks.open());
        let create = format!("CREATE TABLE t ({COLUMNS})");
        with.execute(&create).unwrap();
        plain.execute(&create).unwrap();
        for (name, columns) in &INDEXES[..3] {
            with.execute(&format!("CREATE INDEX {name} ON t ({columns})")).unwrap();
        }

        for op in &ops {
            match op {
                Op::Insert(sql) | Op::Write(sql) | Op::Select(sql) => {
                    let (a, b) = (run(&mut with, sql), run(&mut plain, sql));
                    prop_assert_eq!(a, b, "{}", sql);
                }
                Op::Txn(sql) => {
                    let (a, b) = (run(&mut with, sql), run(&mut plain, sql));
                    prop_assert_eq!(a, b, "{}", sql);
                }
                Op::CreateIndex(i) => {
                    let (name, columns) = INDEXES[*i];
                    run(&mut with, &format!("CREATE INDEX IF NOT EXISTS {name} ON t ({columns})"));
                }
                Op::DropIndex(i) => {
                    run(&mut with, &format!("DROP INDEX IF EXISTS {}", INDEXES[*i].0));
                }
                Op::Crash => {
                    drop(with);
                    drop(plain);
                    with = with_disks.open();
                    plain = plain_disks.open();
                }
            }
            if !with.in_transaction() {
                with.check().unwrap();
            }
        }

        let everything = "SELECT * FROM t";
        prop_assert_eq!(run(&mut with, everything), run(&mut plain, everything));
    }
}

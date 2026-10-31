//! `CREATE INDEX`, `DROP INDEX` and `EXPLAIN`.

use super::{CreateIndex, Statement, parse_one};
use crate::error::Error;

#[test]
fn parses_create_index() {
    assert_eq!(
        parse_one("CREATE INDEX by_age ON users (age, Name)").unwrap(),
        Statement::CreateIndex(CreateIndex {
            name: "by_age".into(),
            if_not_exists: false,
            table: "users".into(),
            columns: vec!["age".into(), "name".into()],
        })
    );
    let Statement::CreateIndex(def) = parse_one("create index if not exists i on t (a)").unwrap()
    else {
        panic!("not CREATE INDEX");
    };
    assert!(def.if_not_exists);
}

#[test]
fn parses_drop_index() {
    assert_eq!(
        parse_one("DROP INDEX by_age").unwrap(),
        Statement::DropIndex {
            name: "by_age".into(),
            if_exists: false
        }
    );
    assert_eq!(
        parse_one("drop index if exists by_age;").unwrap(),
        Statement::DropIndex {
            name: "by_age".into(),
            if_exists: true
        }
    );
}

#[test]
fn prints_as_canonical_sql() {
    for sql in [
        "CREATE INDEX i ON t (a)",
        "CREATE INDEX IF NOT EXISTS i ON t (a, b)",
        "DROP INDEX i",
        "DROP INDEX IF EXISTS i",
        "EXPLAIN SELECT * FROM t WHERE a = 1",
        "EXPLAIN UPDATE t SET a = 1 WHERE b > 2",
        "EXPLAIN DELETE FROM t WHERE a < 3",
    ] {
        assert_eq!(parse_one(sql).unwrap().to_string(), sql);
    }
}

#[test]
fn explain_takes_only_statements_with_a_plan() {
    let Statement::Explain(inner) = parse_one("EXPLAIN SELECT 1").unwrap() else {
        panic!("not EXPLAIN");
    };
    assert!(matches!(*inner, Statement::Select(_)));
    for sql in [
        "EXPLAIN",
        "EXPLAIN INSERT INTO t VALUES (1)",
        "EXPLAIN BEGIN",
        "EXPLAIN EXPLAIN SELECT 1",
        "EXPLAIN CREATE INDEX i ON t (a)",
    ] {
        assert!(
            matches!(parse_one(sql), Err(Error::Syntax { .. })),
            "{sql} should not parse"
        );
    }
}

#[test]
fn bad_definitions_are_syntax_errors() {
    for sql in [
        "CREATE INDEX i",
        "CREATE INDEX i ON t",
        "CREATE INDEX i ON t ()",
        "CREATE INDEX ON t (a)",
        "CREATE INDEX i FROM t (a)",
        "DROP INDEX",
    ] {
        assert!(
            matches!(parse_one(sql), Err(Error::Syntax { .. })),
            "{sql} should not parse"
        );
    }
}

#[test]
fn the_words_are_not_reserved() {
    // The catalog re-parses stored CREATE TABLE text, so reserving these
    // would stop databases that use them as names from opening.
    let sql = "CREATE TABLE index (on INT, explain TEXT)";
    assert_eq!(parse_one(sql).unwrap().to_string(), sql);
    parse_one("SELECT on, explain FROM index").unwrap();
    parse_one("CREATE INDEX on ON index (on)").unwrap();
}

//! `BEGIN`, `COMMIT` and `ROLLBACK`.

use super::{Statement, parse, parse_one};
use crate::error::Error;

#[test]
fn parses_the_three_statements() {
    assert_eq!(parse_one("BEGIN").unwrap(), Statement::Begin);
    assert_eq!(parse_one("begin transaction;").unwrap(), Statement::Begin);
    assert_eq!(parse_one("Commit;").unwrap(), Statement::Commit);
    assert_eq!(parse_one("ROLLBACK").unwrap(), Statement::Rollback);
}

#[test]
fn prints_them_as_canonical_sql() {
    for sql in ["BEGIN", "COMMIT", "ROLLBACK"] {
        assert_eq!(parse_one(sql).unwrap().to_string(), sql);
    }
}

#[test]
fn they_can_share_a_script_with_other_statements() {
    let statements = parse("BEGIN; DELETE FROM t; ROLLBACK;").unwrap();
    assert_eq!(statements.len(), 3);
    assert_eq!(statements[0], Statement::Begin);
    assert_eq!(statements[2], Statement::Rollback);
}

#[test]
fn the_words_are_not_reserved() {
    // A catalog stores CREATE TABLE text and parses it on open, so reserving
    // these words would make older databases with such names fail to open.
    let sql = "CREATE TABLE begin (commit INT, rollback TEXT, transaction BOOL)";
    assert_eq!(parse_one(sql).unwrap().to_string(), sql);
    parse_one("SELECT commit, rollback FROM begin WHERE transaction").unwrap();
    parse_one("UPDATE begin SET commit = 1").unwrap();
}

#[test]
fn extra_words_are_syntax_errors() {
    for sql in ["BEGIN WORK", "COMMIT 1", "ROLLBACK TO x"] {
        assert!(
            matches!(parse_one(sql), Err(Error::Syntax { .. })),
            "{sql} should not parse"
        );
    }
}

#[test]
fn an_unknown_word_still_asks_for_a_statement() {
    let err = parse_one("beginning").unwrap_err().to_string();
    assert!(err.contains("expected a statement"), "{err}");
}

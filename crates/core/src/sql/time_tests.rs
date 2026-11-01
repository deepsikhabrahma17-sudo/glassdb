use super::*;
use crate::error::Error;
use crate::time::parse_timestamp;

fn select(sql: &str) -> Select {
    match parse_one(sql).unwrap() {
        Statement::Select(select) => select,
        other => panic!("not a SELECT: {other}"),
    }
}

fn time(text: &str) -> i64 {
    parse_timestamp(text).unwrap()
}

#[test]
fn plain_selects_read_the_present() {
    assert_eq!(select("SELECT * FROM t").when, When::Now);
    assert_eq!(select("SELECT 1").when, When::Now);
}

#[test]
fn as_of_takes_a_txn() {
    assert_eq!(select("SELECT * FROM t AS OF TXN 412").when, When::Txn(412));
    assert_eq!(select("select * from t as of txn 0").when, When::Txn(0));
}

#[test]
fn as_of_takes_a_timestamp() {
    assert_eq!(
        select("SELECT * FROM t AS OF '2026-11-01 10:00'").when,
        When::Time(time("2026-11-01 10:00"))
    );
}

#[test]
fn history_follows_the_table() {
    let s = select("SELECT * FROM accounts HISTORY WHERE id = 7");
    assert_eq!(s.when, When::History);
    assert!(s.filter.is_some());
}

#[test]
fn the_other_clauses_still_follow() {
    let s = select("SELECT a FROM t AS OF TXN 1 WHERE a > 1 ORDER BY a LIMIT 1");
    assert_eq!(s.when, When::Txn(1));
    assert!(s.filter.is_some());
    assert_eq!(s.order_by.len(), 1);
    assert!(s.limit.is_some());
}

#[test]
fn vacuum_parses() {
    assert_eq!(
        parse_one("VACUUM HISTORY BEFORE '2026-10-01'").unwrap(),
        Statement::Vacuum {
            before: time("2026-10-01")
        }
    );
}

#[test]
fn they_print_back_as_sql() {
    for sql in [
        "SELECT * FROM t AS OF TXN 412",
        "SELECT * FROM t AS OF '2026-11-01 10:00:00'",
        "SELECT * FROM t AS OF '2026-11-01 10:00:00.000001' WHERE a = 1",
        "SELECT * FROM t HISTORY WHERE id = 7",
        "VACUUM HISTORY BEFORE '2026-10-01 00:00:00'",
        "EXPLAIN SELECT * FROM t AS OF TXN 1",
    ] {
        let printed = parse_one(sql).unwrap().to_string();
        assert_eq!(printed, sql);
    }
}

#[test]
fn the_new_words_stay_usable_as_names() {
    let sql = "CREATE TABLE history (of INT, txn INT, vacuum TEXT, before BOOL)";
    assert_eq!(parse_one(sql).unwrap().to_string(), sql);
    assert_eq!(
        select("SELECT of, txn FROM history").from.unwrap(),
        "history"
    );
    assert_eq!(select("SELECT * FROM history HISTORY").when, When::History);
    assert_eq!(select("SELECT * FROM of AS OF TXN 1").when, When::Txn(1));
}

fn syntax_error(sql: &str) -> String {
    match parse_one(sql) {
        Err(err @ Error::Syntax { .. }) => err.to_string(),
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn bad_clauses_are_syntax_errors() {
    assert!(syntax_error("SELECT * FROM t AS TXN 1").contains("OF"));
    assert!(syntax_error("SELECT * FROM t AS OF").contains("TXN or a timestamp"));
    assert!(syntax_error("SELECT * FROM t AS OF TXN").contains("transaction id"));
    assert!(syntax_error("SELECT * FROM t AS OF TXN -1").contains("transaction id"));
    assert!(syntax_error("SELECT * FROM t AS OF TXN 1.5").contains("transaction id"));
    assert!(
        syntax_error("SELECT * FROM t AS OF TXN 99999999999999999999").contains("transaction id")
    );
    assert!(syntax_error("SELECT * FROM t AS OF 'soon'").contains("timestamp"));
    assert!(syntax_error("SELECT * FROM t AS OF TXN 1 HISTORY").contains("HISTORY"));
    assert!(syntax_error("VACUUM").contains("HISTORY"));
    assert!(syntax_error("VACUUM HISTORY").contains("BEFORE"));
    assert!(syntax_error("VACUUM HISTORY BEFORE 5").contains("timestamp"));
    assert!(syntax_error("VACUUM HISTORY BEFORE 'then'").contains("timestamp"));
}

#[test]
fn only_a_select_can_look_back() {
    syntax_error("DELETE FROM t AS OF TXN 1");
    syntax_error("UPDATE t AS OF TXN 1 SET a = 1");
    syntax_error("SELECT 1 AS OF TXN 1");
}

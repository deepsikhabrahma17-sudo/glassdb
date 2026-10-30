//! Tokenizer and parser tests.

use super::ast::*;
use super::token::{Keyword, Token, tokenize};
use super::{parse, parse_one};
use crate::error::Error;
use crate::types::{DataType, Value};

fn tokens(sql: &str) -> Vec<Token> {
    tokenize(sql)
        .unwrap()
        .into_iter()
        .map(|t| t.token)
        .collect()
}

/// The (message, line, column) of a syntax error.
fn syntax_error(sql: &str) -> (String, usize, usize) {
    match parse(sql) {
        Err(Error::Syntax {
            message,
            line,
            column,
        }) => (message, line, column),
        other => panic!("expected a syntax error for {sql:?}, got {other:?}"),
    }
}

fn col(name: &str) -> Expr {
    Expr::Column(name.into())
}

fn int(n: i64) -> Expr {
    Expr::Literal(Value::Int(n))
}

fn text(s: &str) -> Expr {
    Expr::Literal(Value::Text(s.into()))
}

fn bin(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

fn unary(op: UnaryOp, expr: Expr) -> Expr {
    Expr::Unary {
        op,
        expr: Box::new(expr),
    }
}

fn is_null(expr: Expr, negated: bool) -> Expr {
    Expr::IsNull {
        expr: Box::new(expr),
        negated,
    }
}

/// The first select item of `SELECT <sql>`.
fn expr(sql: &str) -> Expr {
    match parse_one(&format!("SELECT {sql}")).unwrap() {
        Statement::Select(select) => match &select.items[0] {
            SelectItem::Expr { expr, .. } => expr.clone(),
            other => panic!("not an expression: {other:?}"),
        },
        other => panic!("not a SELECT: {other:?}"),
    }
}

// ---- tokenizer ----

#[test]
fn keywords_in_any_case_and_identifiers_lowercased() {
    assert_eq!(
        tokens("select FROM Users"),
        vec![
            Token::Keyword(Keyword::Select),
            Token::Keyword(Keyword::From),
            Token::Ident("users".into()),
            Token::Eof
        ]
    );
}

#[test]
fn numbers_strings_and_symbols() {
    use Token::*;
    assert_eq!(
        tokens("42 1.5 2e3 'it''s' <> != <= >= = < > ( ) , ; * + - / %"),
        vec![
            Number("42".into()),
            Number("1.5".into()),
            Number("2e3".into()),
            Text("it's".into()),
            NotEq,
            NotEq,
            LtEq,
            GtEq,
            Eq,
            Lt,
            Gt,
            LParen,
            RParen,
            Comma,
            Semicolon,
            Star,
            Plus,
            Minus,
            Slash,
            Percent,
            Eof
        ]
    );
}

#[test]
fn comments_are_skipped() {
    assert_eq!(
        tokens("SELECT -- the answer\n  42"),
        vec![
            Token::Keyword(Keyword::Select),
            Token::Number("42".into()),
            Token::Eof
        ]
    );
}

#[test]
fn positions_count_lines_and_columns() {
    let toks = tokenize("SELECT\n  x").unwrap();
    assert_eq!((toks[0].line, toks[0].column), (1, 1));
    assert_eq!((toks[1].line, toks[1].column), (2, 3));
}

#[test]
fn tokenizer_errors_point_at_the_problem() {
    let (msg, line, column) = syntax_error("SELECT 'oops");
    assert!(msg.contains("unterminated string"), "{msg}");
    assert_eq!((line, column), (1, 8));

    let (msg, _, _) = syntax_error("SELECT \"name\"");
    assert!(msg.contains("use single quotes"), "{msg}");

    let (msg, line, column) = syntax_error("SELECT\n  @");
    assert_eq!(msg, "unexpected character '@'");
    assert_eq!((line, column), (2, 3));
}

// ---- parser: statements ----

#[test]
fn create_table() {
    let sql =
        "create table if not exists Users (id int primary key, name TEXT, score real, ok boolean);";
    let column = |name: &str, data_type, primary_key| ColumnDef {
        name: name.into(),
        data_type,
        primary_key,
    };
    assert_eq!(
        parse_one(sql).unwrap(),
        Statement::CreateTable(CreateTable {
            name: "users".into(),
            if_not_exists: true,
            columns: vec![
                column("id", DataType::Int, true),
                column("name", DataType::Text, false),
                column("score", DataType::Real, false),
                column("ok", DataType::Bool, false),
            ],
        })
    );
}

#[test]
fn drop_table() {
    assert_eq!(
        parse_one("DROP TABLE IF EXISTS t").unwrap(),
        Statement::DropTable {
            name: "t".into(),
            if_exists: true
        }
    );
}

#[test]
fn insert() {
    assert_eq!(
        parse_one("INSERT INTO t (a, b) VALUES (1, 'x'), (2, NULL)").unwrap(),
        Statement::Insert {
            table: "t".into(),
            columns: Some(vec!["a".into(), "b".into()]),
            rows: vec![
                vec![int(1), text("x")],
                vec![int(2), Expr::Literal(Value::Null)]
            ],
        }
    );
}

#[test]
fn select_with_every_clause() {
    let sql = "SELECT a, b + 1 AS c, * FROM t WHERE a > 1 ORDER BY a DESC, b LIMIT 10 OFFSET 5";
    assert_eq!(
        parse_one(sql).unwrap(),
        Statement::Select(Select {
            items: vec![
                SelectItem::Expr {
                    expr: col("a"),
                    alias: None
                },
                SelectItem::Expr {
                    expr: bin(col("b"), BinaryOp::Add, int(1)),
                    alias: Some("c".into())
                },
                SelectItem::Wildcard,
            ],
            from: Some("t".into()),
            filter: Some(bin(col("a"), BinaryOp::Gt, int(1))),
            order_by: vec![
                OrderBy {
                    expr: col("a"),
                    descending: true
                },
                OrderBy {
                    expr: col("b"),
                    descending: false
                },
            ],
            limit: Some(int(10)),
            offset: Some(int(5)),
        })
    );
}

#[test]
fn select_without_from_and_alias_without_as() {
    let Statement::Select(select) = parse_one("SELECT 1 one").unwrap() else {
        panic!("not a SELECT")
    };
    assert_eq!(select.from, None);
    assert_eq!(
        select.items,
        vec![SelectItem::Expr {
            expr: int(1),
            alias: Some("one".into())
        }]
    );
}

#[test]
fn update_and_delete() {
    assert_eq!(
        parse_one("UPDATE t SET a = a + 1, b = 'x' WHERE a IS NOT NULL").unwrap(),
        Statement::Update {
            table: "t".into(),
            assignments: vec![
                ("a".into(), bin(col("a"), BinaryOp::Add, int(1))),
                ("b".into(), text("x")),
            ],
            filter: Some(is_null(col("a"), true)),
        }
    );
    assert_eq!(
        parse_one("DELETE FROM t").unwrap(),
        Statement::Delete {
            table: "t".into(),
            filter: None
        }
    );
}

// ---- parser: expressions ----

#[test]
fn operator_precedence() {
    use BinaryOp::*;
    assert_eq!(
        expr("1 + 2 * 3"),
        bin(int(1), Add, bin(int(2), Mul, int(3)))
    );
    assert_eq!(
        expr("(1 + 2) * 3"),
        bin(bin(int(1), Add, int(2)), Mul, int(3))
    );
    assert_eq!(
        expr("10 - 2 - 3"),
        bin(bin(int(10), Sub, int(2)), Sub, int(3))
    );
    assert_eq!(
        expr("a OR b AND c"),
        bin(col("a"), Or, bin(col("b"), And, col("c")))
    );
    assert_eq!(
        expr("NOT a = 1 AND b"),
        bin(
            unary(UnaryOp::Not, bin(col("a"), Eq, int(1))),
            And,
            col("b")
        )
    );
    assert_eq!(
        expr("-x * 2"),
        bin(unary(UnaryOp::Neg, col("x")), Mul, int(2))
    );
    assert_eq!(
        expr("a IS NOT NULL OR b IS NULL"),
        bin(is_null(col("a"), true), Or, is_null(col("b"), false))
    );
}

#[test]
fn negative_number_literals_are_folded() {
    assert_eq!(expr("-5"), int(-5));
    assert_eq!(expr("- (2)"), int(-2));
    assert_eq!(expr("-9223372036854775808"), int(i64::MIN));
    assert_eq!(expr("-1.5"), Expr::Literal(Value::Real(-1.5)));
    assert_eq!(expr("2e3"), Expr::Literal(Value::Real(2000.0)));
}

#[test]
fn syntax_errors_say_what_and_where() {
    assert_eq!(
        syntax_error("SELECT FROM t"),
        ("expected an expression, found FROM".into(), 1, 8)
    );
    assert_eq!(
        syntax_error("SELECT * FROM t WHERE"),
        ("expected an expression, found end of input".into(), 1, 22)
    );
    let cases = [
        ("CREATE TABLE t (a FLOAT)", "unknown type float"),
        ("INSERT INTO t VALUES (1", "expected ')'"),
        ("SELECT 9223372036854775808", "integer too large"),
        (
            "SELECT 1 SELECT 2",
            "expected ';' or end of input, found SELECT",
        ),
        ("DROP t", "expected TABLE, found t"),
        ("UPDATE t SET a", "expected '='"),
    ];
    for (sql, want) in cases {
        let (msg, _, _) = syntax_error(sql);
        assert!(msg.contains(want), "{sql}: {msg}");
    }
}

#[test]
fn several_statements_in_one_string() {
    let stmts =
        parse("CREATE TABLE t (a INT); INSERT INTO t VALUES (1);; SELECT * FROM t").unwrap();
    assert_eq!(stmts.len(), 3);
    assert!(parse("  -- nothing here\n ;").unwrap().is_empty());
}

#[test]
fn parse_one_wants_exactly_one_statement() {
    assert!(parse_one("SELECT 1; SELECT 2").is_err());
    assert!(parse_one("").is_err());
    assert!(parse_one("SELECT 1;").is_ok());
}

// ---- printing back ----

#[test]
fn printing_gives_back_the_same_statement() {
    let statements = [
        "CREATE TABLE IF NOT EXISTS t (id INT PRIMARY KEY, name TEXT, x REAL, ok BOOL)",
        "DROP TABLE t",
        "INSERT INTO t VALUES (1, 'it''s', -2.5, TRUE), (2, NULL, 1e3, FALSE)",
        "INSERT INTO t (a) VALUES (-9223372036854775808)",
        "SELECT * FROM t WHERE NOT (a = 1 OR b < 2) AND c IS NOT NULL ORDER BY a DESC, b LIMIT 3 OFFSET 1",
        "SELECT 2 * (3 + 4), 1 - -5, -(a + b), (a - b) - c, a - (b - c), -x AS neg",
        "UPDATE t SET a = a % 7, b = 'x' WHERE a <> 3",
        "DELETE FROM t WHERE a >= 1 AND a <= 2",
    ];
    for sql in statements {
        let stmt = parse_one(sql).unwrap();
        let printed = stmt.to_string();
        assert_eq!(
            parse_one(&printed).unwrap(),
            stmt,
            "{sql}\nprinted as {printed}"
        );
    }
}

#[test]
fn printed_sql_is_tidy() {
    assert_eq!(
        parse_one("create table Users(id int primary key,name text)")
            .unwrap()
            .to_string(),
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)"
    );
    assert_eq!(
        parse_one("select 2*(3+4), 1+2*3, -(2+3), 10-(2-3), not (a and b), 'it''s', 3 != 4 from t")
            .unwrap()
            .to_string(),
        "SELECT 2 * (3 + 4), 1 + 2 * 3, -(2 + 3), 10 - (2 - 3), NOT (a AND b), 'it''s', 3 <> 4 FROM t"
    );
}

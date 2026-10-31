//! The syntax tree: what the parser produces and the executor runs.
//!
//! Every node prints back as tidy SQL (`Display`). The catalog stores tables
//! as printed `CREATE TABLE` statements, and SELECT column headers are
//! printed expressions, so printing must round-trip through the parser.

use std::fmt;

use crate::types::{DataType, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable {
        name: String,
        if_exists: bool,
    },
    Insert {
        table: String,
        /// The listed columns, or `None` for "every column, in order".
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Expr>>,
    },
    Select(Select),
    Update {
        table: String,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
    Delete {
        table: String,
        filter: Option<Expr>,
    },
    Begin,
    Commit,
    Rollback,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub primary_key: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub items: Vec<SelectItem>,
    pub from: Option<String>,
    pub filter: Option<Expr>,
    pub order_by: Vec<OrderBy>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`: every column of the table.
    Wildcard,
    Expr {
        expr: Expr,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderBy {
    pub expr: Expr,
    pub descending: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    Column(String),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        left: Box<Expr>,
        op: BinaryOp,
        right: Box<Expr>,
    },
    /// `expr IS NULL`, or `expr IS NOT NULL` when `negated`.
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        use BinaryOp::*;
        match self {
            Add => "+",
            Sub => "-",
            Mul => "*",
            Div => "/",
            Rem => "%",
            Eq => "=",
            NotEq => "<>",
            Lt => "<",
            LtEq => "<=",
            Gt => ">",
            GtEq => ">=",
            And => "AND",
            Or => "OR",
        }
    }

    /// Binding strength: higher binds tighter. Matches the parser's levels.
    fn precedence(self) -> u8 {
        use BinaryOp::*;
        match self {
            Or => 1,
            And => 2,
            Eq | NotEq | Lt | LtEq | Gt | GtEq => COMPARISON,
            Add | Sub => 5,
            Mul | Div | Rem => 6,
        }
    }
}

const NOT: u8 = 3;
const COMPARISON: u8 = 4;
const NEG: u8 = 7;
const ATOM: u8 = 8;

impl Expr {
    fn precedence(&self) -> u8 {
        match self {
            Expr::Binary { op, .. } => op.precedence(),
            Expr::Unary {
                op: UnaryOp::Not, ..
            } => NOT,
            Expr::IsNull { .. } => COMPARISON,
            Expr::Unary {
                op: UnaryOp::Neg, ..
            } => NEG,
            _ if self.starts_with_minus() => NEG,
            _ => ATOM,
        }
    }

    /// Whether this prints with a leading `-`. Printing `-` before it again
    /// would make `--`, which starts a comment, so it gets parentheses.
    fn starts_with_minus(&self) -> bool {
        match self {
            Expr::Literal(Value::Int(n)) => *n < 0,
            Expr::Literal(Value::Real(r)) => r.is_sign_negative(),
            Expr::Unary {
                op: UnaryOp::Neg, ..
            } => true,
            _ => false,
        }
    }
}

/// Write `expr`, in parentheses if `wrap`.
fn sub(f: &mut fmt::Formatter<'_>, expr: &Expr, wrap: bool) -> fmt::Result {
    if wrap {
        write!(f, "({expr})")
    } else {
        write!(f, "{expr}")
    }
}

impl fmt::Display for Expr {
    /// Prints only the parentheses needed to keep the same tree.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expr::Literal(value) => write!(f, "{}", value.to_sql()),
            Expr::Column(name) => write!(f, "{name}"),
            Expr::Unary {
                op: UnaryOp::Not,
                expr,
            } => {
                write!(f, "NOT ")?;
                sub(f, expr, expr.precedence() < NOT)
            }
            Expr::Unary {
                op: UnaryOp::Neg,
                expr,
            } => {
                write!(f, "-")?;
                sub(f, expr, expr.precedence() < NEG || expr.starts_with_minus())
            }
            Expr::IsNull { expr, negated } => {
                sub(f, expr, expr.precedence() <= COMPARISON)?;
                write!(f, " IS {}NULL", if *negated { "NOT " } else { "" })
            }
            Expr::Binary { left, op, right } => {
                let p = op.precedence();
                // Operators group left to right, so an equal-strength operator
                // needs parentheses on the right (and on both sides for
                // comparisons, which don't chain at all).
                sub(
                    f,
                    left,
                    left.precedence() < p || (p == COMPARISON && left.precedence() == p),
                )?;
                write!(f, " {} ", op.symbol())?;
                sub(f, right, right.precedence() <= p)
            }
        }
    }
}

/// Write `items` separated by ", ".
fn list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.data_type)?;
        if self.primary_key {
            write!(f, " PRIMARY KEY")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => write!(f, "*"),
            SelectItem::Expr { expr, alias: None } => write!(f, "{expr}"),
            SelectItem::Expr {
                expr,
                alias: Some(a),
            } => write!(f, "{expr} AS {a}"),
        }
    }
}

impl fmt::Display for OrderBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            self.expr,
            if self.descending { " DESC" } else { "" }
        )
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::CreateTable(c) => {
                let if_not_exists = if c.if_not_exists {
                    "IF NOT EXISTS "
                } else {
                    ""
                };
                write!(f, "CREATE TABLE {if_not_exists}{} (", c.name)?;
                list(f, &c.columns)?;
                write!(f, ")")
            }
            Statement::DropTable { name, if_exists } => {
                let if_exists = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP TABLE {if_exists}{name}")
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                write!(f, "INSERT INTO {table}")?;
                if let Some(columns) = columns {
                    write!(f, " (")?;
                    list(f, columns)?;
                    write!(f, ")")?;
                }
                write!(f, " VALUES ")?;
                for (i, row) in rows.iter().enumerate() {
                    write!(f, "{}(", if i > 0 { ", " } else { "" })?;
                    list(f, row)?;
                    write!(f, ")")?;
                }
                Ok(())
            }
            Statement::Select(s) => {
                write!(f, "SELECT ")?;
                list(f, &s.items)?;
                if let Some(table) = &s.from {
                    write!(f, " FROM {table}")?;
                }
                if let Some(filter) = &s.filter {
                    write!(f, " WHERE {filter}")?;
                }
                if !s.order_by.is_empty() {
                    write!(f, " ORDER BY ")?;
                    list(f, &s.order_by)?;
                }
                if let Some(limit) = &s.limit {
                    write!(f, " LIMIT {limit}")?;
                }
                if let Some(offset) = &s.offset {
                    write!(f, " OFFSET {offset}")?;
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                filter,
            } => {
                write!(f, "UPDATE {table} SET ")?;
                for (i, (column, expr)) in assignments.iter().enumerate() {
                    write!(f, "{}{column} = {expr}", if i > 0 { ", " } else { "" })?;
                }
                if let Some(filter) = filter {
                    write!(f, " WHERE {filter}")?;
                }
                Ok(())
            }
            Statement::Delete { table, filter } => {
                write!(f, "DELETE FROM {table}")?;
                if let Some(filter) = filter {
                    write!(f, " WHERE {filter}")?;
                }
                Ok(())
            }
            Statement::Begin => write!(f, "BEGIN"),
            Statement::Commit => write!(f, "COMMIT"),
            Statement::Rollback => write!(f, "ROLLBACK"),
        }
    }
}

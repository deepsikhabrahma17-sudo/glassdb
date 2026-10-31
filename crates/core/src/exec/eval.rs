//! Evaluating expressions against one row.
//!
//! NULL means "unknown", and the rules follow from that: `NULL + 1` is NULL,
//! `NULL = NULL` is NULL (not true), `NULL AND FALSE` is false (false
//! whatever the unknown is), and `WHERE` keeps a row only if it is true.

use std::cmp::Ordering;

use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::sql::{BinaryOp, Expr, UnaryOp};
use crate::types::Value;

/// What an expression can see: the table's columns and the current row.
pub(crate) struct Scope<'a> {
    pub schema: Option<&'a TableSchema>,
    pub row: &'a [Value],
}

impl Scope<'_> {
    /// No table: only literals and operators (for VALUES, LIMIT, `SELECT 1`).
    pub fn empty() -> Scope<'static> {
        Scope {
            schema: None,
            row: &[],
        }
    }
}

fn query<T>(msg: String) -> Result<T> {
    Err(Error::Query(msg))
}

/// Check that every column `expr` names exists, before any row is read, so
/// a typo fails even on an empty table.
pub(crate) fn check_columns(expr: &Expr, schema: Option<&TableSchema>) -> Result<()> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) => match schema.and_then(|s| s.column_index(name)) {
            Some(_) => Ok(()),
            None => query(format!("no such column: {name}")),
        },
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => check_columns(expr, schema),
        Expr::Binary { left, right, .. } => {
            check_columns(left, schema)?;
            check_columns(right, schema)
        }
    }
}

pub(crate) fn eval(expr: &Expr, scope: &Scope) -> Result<Value> {
    match expr {
        Expr::Literal(value) => Ok(value.clone()),
        Expr::Column(name) => match scope.schema.and_then(|s| s.column_index(name)) {
            Some(i) => Ok(scope.row[i].clone()),
            None => query(format!("no such column: {name}")),
        },
        Expr::IsNull { expr, negated } => {
            let is_null = eval(expr, scope)? == Value::Null;
            Ok(Value::Bool(is_null != *negated))
        }
        Expr::Unary {
            op: UnaryOp::Neg,
            expr,
        } => match eval(expr, scope)? {
            Value::Null => Ok(Value::Null),
            Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(overflow),
            Value::Real(r) => Ok(Value::Real(-r)),
            other => query(format!("cannot apply - to {}", other.type_name())),
        },
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => match eval(expr, scope)? {
            Value::Null => Ok(Value::Null),
            Value::Bool(b) => Ok(Value::Bool(!b)),
            other => query(format!("NOT needs a BOOL, got {}", other.type_name())),
        },
        Expr::Binary { left, op, right } => {
            let (l, r) = (eval(left, scope)?, eval(right, scope)?);
            match op {
                BinaryOp::And | BinaryOp::Or => logic(*op, &l, &r),
                BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem => {
                    arithmetic(*op, &l, &r)
                }
                _ => comparison(*op, &l, &r),
            }
        }
    }
}

/// Whether a WHERE result keeps the row: only TRUE does; FALSE and NULL don't.
pub(crate) fn is_true(value: &Value, clause: &str) -> Result<bool> {
    match value {
        Value::Bool(b) => Ok(*b),
        Value::Null => Ok(false),
        other => query(format!("{clause} must be BOOL, got {}", other.type_name())),
    }
}

/// AND / OR with NULL as "unknown".
fn logic(op: BinaryOp, l: &Value, r: &Value) -> Result<Value> {
    let as_bool = |v: &Value| match v {
        Value::Bool(b) => Ok(Some(*b)),
        Value::Null => Ok(None),
        other => query(format!(
            "{} needs BOOL values, got {}",
            op.symbol(),
            other.type_name()
        )),
    };
    let (a, b) = (as_bool(l)?, as_bool(r)?);
    // The deciding value wins even against unknown: FALSE for AND, TRUE for OR.
    let decider = op == BinaryOp::Or;
    Ok(if a == Some(decider) || b == Some(decider) {
        Value::Bool(decider)
    } else if a.is_none() || b.is_none() {
        Value::Null
    } else {
        Value::Bool(!decider)
    })
}

fn comparison(op: BinaryOp, l: &Value, r: &Value) -> Result<Value> {
    let Some(ordering) = l.compare(r)? else {
        return Ok(Value::Null);
    };
    let result = match op {
        BinaryOp::Eq => ordering == Ordering::Equal,
        BinaryOp::NotEq => ordering != Ordering::Equal,
        BinaryOp::Lt => ordering == Ordering::Less,
        BinaryOp::LtEq => ordering != Ordering::Greater,
        BinaryOp::Gt => ordering == Ordering::Greater,
        BinaryOp::GtEq => ordering != Ordering::Less,
        _ => unreachable!("not a comparison: {op:?}"),
    };
    Ok(Value::Bool(result))
}

/// `+ - * / %`. Two INTs give an INT (with overflow checks); a REAL on
/// either side gives a REAL. Dividing by zero is an error either way.
fn arithmetic(op: BinaryOp, l: &Value, r: &Value) -> Result<Value> {
    match (l, r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => {
            let (a, b) = (*a, *b);
            if matches!(op, BinaryOp::Div | BinaryOp::Rem) && b == 0 {
                return query("division by zero".into());
            }
            let result = match op {
                BinaryOp::Add => a.checked_add(b),
                BinaryOp::Sub => a.checked_sub(b),
                BinaryOp::Mul => a.checked_mul(b),
                BinaryOp::Div => a.checked_div(b),
                _ => a.checked_rem(b),
            };
            result.map(Value::Int).ok_or_else(overflow)
        }
        (Value::Int(_) | Value::Real(_), Value::Int(_) | Value::Real(_)) => {
            let (a, b) = (as_f64(l), as_f64(r));
            if matches!(op, BinaryOp::Div | BinaryOp::Rem) && b == 0.0 {
                return query("division by zero".into());
            }
            Ok(Value::Real(match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                _ => a % b,
            }))
        }
        _ => query(format!(
            "cannot apply {} to {} and {}",
            op.symbol(),
            l.type_name(),
            r.type_name()
        )),
    }
}

fn as_f64(value: &Value) -> f64 {
    match value {
        Value::Int(n) => *n as f64,
        Value::Real(r) => *r,
        _ => unreachable!("only called on numbers"),
    }
}

fn overflow() -> Error {
    Error::Query("integer overflow".into())
}

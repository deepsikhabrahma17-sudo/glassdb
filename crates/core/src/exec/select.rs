//! SELECT: scan the table, keep rows that pass WHERE, compute the output
//! columns, sort, then apply OFFSET and LIMIT.
//!
//! Every SELECT reads the whole table for now; using indexes to read less
//! is the query planner's job, which comes later.

use std::cmp::Ordering;

use super::eval::{Scope, check_columns, eval, is_true};
use super::{Database, Output};
use crate::btree::BTree;
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::pager::Pager;
use crate::sql::{Expr, Select, SelectItem};
use crate::types::{Value, decode_row};

pub(super) fn select(db: &mut Database, s: &Select) -> Result<Output> {
    let schema = match &s.from {
        Some(name) => Some(db.table(name)?),
        None => None,
    };
    let schema = schema.as_ref();

    // Output column names: `*` expands to the table's columns; an expression
    // is named by its alias or by its own SQL text.
    let mut columns = Vec::new();
    for item in &s.items {
        match item {
            SelectItem::Wildcard => match schema {
                Some(t) => columns.extend(t.columns.iter().map(|c| c.name.clone())),
                None => return Err(Error::Query("SELECT * needs a FROM clause".into())),
            },
            SelectItem::Expr { expr, alias } => {
                check_columns(expr, schema)?;
                columns.push(alias.clone().unwrap_or_else(|| expr.to_string()));
            }
        }
    }
    if let Some(filter) = &s.filter {
        check_columns(filter, schema)?;
    }
    for order in &s.order_by {
        if output_column(&order.expr, schema, &columns).is_none() {
            check_columns(&order.expr, schema)?;
        }
    }
    let offset = count(s.offset.as_ref(), "OFFSET")?.unwrap_or(0);
    let limit = count(s.limit.as_ref(), "LIMIT")?;

    // Without FROM there is exactly one (empty) row to compute on.
    let source = match schema {
        Some(t) => scan(&mut db.pager, t)?
            .into_iter()
            .map(|(_, row)| row)
            .collect(),
        None => vec![Vec::new()],
    };

    let mut results = Vec::new();
    for row in source {
        let scope = Scope { schema, row: &row };
        if let Some(filter) = &s.filter
            && !is_true(&eval(filter, &scope)?, "WHERE")?
        {
            continue;
        }
        let projected = project(&s.items, &scope)?;
        let mut sort_keys = Vec::with_capacity(s.order_by.len());
        for order in &s.order_by {
            sort_keys.push(match output_column(&order.expr, schema, &columns) {
                Some(i) => projected[i].clone(),
                None => eval(&order.expr, &scope)?,
            });
        }
        results.push((projected, sort_keys));
    }

    if !s.order_by.is_empty() {
        // A stable sort: rows that tie keep their table order.
        results.sort_by(|(_, a), (_, b)| {
            for (i, order) in s.order_by.iter().enumerate() {
                let ordering = a[i].sort_cmp(&b[i]);
                let ordering = if order.descending {
                    ordering.reverse()
                } else {
                    ordering
                };
                if ordering != Ordering::Equal {
                    return ordering;
                }
            }
            Ordering::Equal
        })
    }
    let rows = results
        .into_iter()
        .map(|(row, _)| row)
        .skip(offset)
        .take(limit.unwrap_or(usize::MAX))
        .collect();
    Ok(Output::Rows { columns, rows })
}

/// ORDER BY may name an output column (`SELECT a + 1 AS x ... ORDER BY x`).
/// A real table column of the same name wins.
fn output_column(expr: &Expr, schema: Option<&TableSchema>, columns: &[String]) -> Option<usize> {
    match expr {
        Expr::Column(name) if schema.and_then(|t| t.column_index(name)).is_none() => {
            columns.iter().position(|c| c == name)
        }
        _ => None,
    }
}

fn project(items: &[SelectItem], scope: &Scope) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard => out.extend(scope.row.iter().cloned()),
            SelectItem::Expr { expr, .. } => out.push(eval(expr, scope)?),
        }
    }
    Ok(out)
}

/// LIMIT / OFFSET: a constant, non-negative INT.
fn count(expr: Option<&Expr>, clause: &str) -> Result<Option<usize>> {
    let Some(expr) = expr else { return Ok(None) };
    check_columns(expr, None)?;
    match eval(expr, &Scope::empty())? {
        Value::Int(n) if n >= 0 => Ok(Some(n as usize)),
        _ => Err(Error::Query(format!("{clause} must be a non-negative INT"))),
    }
}

/// Every row of a table in key order, with its key.
pub(super) fn scan(pager: &mut Pager, table: &TableSchema) -> Result<Vec<(Vec<u8>, Vec<Value>)>> {
    let mut rows = Vec::new();
    for entry in BTree::open(table.root).iter(pager)? {
        let (key, bytes) = entry?;
        let row = decode_row(&bytes)?;
        if row.len() != table.columns.len() {
            return Err(Error::Corrupt(format!(
                "a row of {} has {} values for {} columns",
                table.name,
                row.len(),
                table.columns.len()
            )));
        }
        rows.push((key, row));
    }
    Ok(rows)
}

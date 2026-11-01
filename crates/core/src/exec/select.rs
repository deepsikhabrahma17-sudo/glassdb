//! SELECT: scan the table, keep rows that pass WHERE, compute the output
//! columns, sort, then apply OFFSET and LIMIT.
//!
//! The planner picks how to find the rows (see `access`), but WHERE is always
//! applied to them, so that choice never changes the result. `AS OF` and
//! `HISTORY` change only where the rows come from.

use std::cmp::Ordering;

use super::access::{Stored, fetch, plan, row_range, trace_plan};
use super::eval::{Scope, check_columns, eval, is_true};
use super::history;
use super::{Engine, Output};
use crate::btree::BTree;
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::mvcc::{TxnId, rows_at};
use crate::pager::Pager;
use crate::planner::{Access, choose};
use crate::sql::{Expr, Select, SelectItem, When};
use crate::types::Value;

/// Where a SELECT's rows come from.
enum Source {
    /// The table as it is.
    Now,
    /// The table as it was at a transaction.
    At(TxnId),
    /// Every version. Holds the plain table, because `Prepared::schema` has
    /// the extra columns.
    History(TableSchema),
}

/// A SELECT after checking, ready to run.
pub(super) struct Prepared {
    schema: Option<TableSchema>,
    source: Source,
    /// How to find the rows, if there is a table to find them in.
    access: Option<Access>,
    columns: Vec<String>,
    offset: usize,
    limit: Option<usize>,
}

/// Check everything that can be checked before any row is read, and plan.
pub(super) fn prepare(db: &mut Engine, s: &Select) -> Result<Prepared> {
    let (table, source) = match &s.from {
        Some(name) => {
            let (table, source) = resolve(db, name, s.when)?;
            (Some(table), source)
        }
        None => (None, Source::Now),
    };
    let schema = table.as_ref();

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
    let access = match (schema, &source) {
        (Some(t), Source::Now) => Some(plan(db, t, s.filter.as_ref())?),
        // Indexes hold only current rows, so the past can't use them.
        (Some(t), _) => Some(choose(t, &[], s.filter.as_ref())),
        (None, _) => None,
    };
    Ok(Prepared {
        access,
        source,
        schema: table,
        columns,
        offset,
        limit,
    })
}

/// The table a SELECT names, at the moment it asks for.
fn resolve(db: &mut Engine, name: &str, when: When) -> Result<(TableSchema, Source)> {
    let at = match when {
        When::Now => return Ok((db.table(name)?, Source::Now)),
        When::History => {
            let table = db.table(name)?;
            return Ok((history::schema(&table)?, Source::History(table)));
        }
        When::Txn(txn) => txn,
        When::Time(time) => db.commits.txn_at(&mut db.pager, time, db.last.txn)?,
    };
    db.check_past(at)?;
    match db.catalog.table_at(&mut db.pager, name, at)? {
        Some(table) => Ok((table, Source::At(at))),
        None => Err(Error::Query(format!("no table {name} at txn {at}"))),
    }
}

/// The steps of a prepared SELECT, as `EXPLAIN` shows them.
pub(super) fn select_steps(s: &Select, p: &Prepared) -> Vec<String> {
    let mut steps = match (&p.schema, &p.access) {
        (Some(table), Some(access)) => {
            let found = access.describe(&table.name);
            vec![match &p.source {
                Source::Now => found,
                Source::At(txn) => format!("{found} AS OF TXN {txn}"),
                Source::History(_) => format!("{found} HISTORY"),
            }]
        }
        _ => vec!["ONE ROW (no table)".to_string()],
    };
    if let Some(filter) = &s.filter {
        steps.push(format!("FILTER {filter}"));
    }
    if !s.order_by.is_empty() {
        let order: Vec<String> = s.order_by.iter().map(ToString::to_string).collect();
        steps.push(format!("SORT {}", order.join(", ")));
    }
    if p.offset > 0 {
        steps.push(format!("OFFSET {}", p.offset));
    }
    if let Some(limit) = p.limit {
        steps.push(format!("LIMIT {limit}"));
    }
    steps
}

pub(super) fn select(db: &mut Engine, s: &Select) -> Result<Output> {
    let prepared = prepare(db, s)?;
    trace_plan(db, select_steps(s, &prepared));
    let Prepared {
        schema: table,
        source,
        access,
        columns,
        offset,
        limit,
    } = prepared;
    let schema = table.as_ref();

    // Without FROM there is exactly one (empty) row to compute on.
    let rows = match (schema, &access, source) {
        (Some(t), Some(access), Source::Now) => fetch(&mut db.pager, t, access)?
            .into_iter()
            .map(|stored| stored.row)
            .collect(),
        (Some(t), Some(access), Source::At(at)) => {
            rows_at(&mut db.pager, t, &row_range(access), at)?
        }
        (_, Some(access), Source::History(table)) => history::rows(db, &table, &row_range(access))?,
        _ => vec![Vec::new()],
    };

    let mut results = Vec::new();
    for row in rows {
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

/// Every current row of a table in key order.
pub(super) fn scan(pager: &mut Pager, table: &TableSchema) -> Result<Vec<Stored>> {
    let mut rows = Vec::new();
    for entry in BTree::open(table.root).iter(pager)? {
        let (key, bytes) = entry?;
        rows.push(Stored::decode(table, key, &bytes)?);
    }
    Ok(rows)
}

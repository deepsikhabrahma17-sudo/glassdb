//! INSERT, UPDATE and DELETE.
//!
//! Each works in two passes: first compute and check every row it will
//! write (types, primary keys, sizes), then write. If the first pass finds a
//! problem, nothing has been written yet.

use std::collections::HashSet;

use super::eval::{Scope, check_columns, eval, is_true};
use super::select::scan;
use super::{Database, Output};
use crate::btree::{BTree, MAX_ENTRY};
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::sql::Expr;
use crate::types::{Value, decode_key, encode_key, encode_row};

/// Table keys are 8-byte row ids, which leaves this much room for the row.
const MAX_ROW: usize = MAX_ENTRY - 8;

fn query<T>(msg: String) -> Result<T> {
    Err(Error::Query(msg))
}

pub(super) fn insert(
    db: &mut Database,
    table: &str,
    columns: Option<&[String]>,
    rows: &[Vec<Expr>],
) -> Result<Output> {
    let schema = db.table(table)?;
    // Which column each VALUES position fills.
    let targets: Vec<usize> = match columns {
        None => (0..schema.columns.len()).collect(),
        Some(names) => {
            let mut targets = Vec::new();
            for name in names {
                let Some(i) = schema.column_index(name) else {
                    return query(format!("no such column: {name}"));
                };
                if targets.contains(&i) {
                    return query(format!("duplicate column {name} in INSERT"));
                }
                targets.push(i);
            }
            targets
        }
    };
    let tree = BTree::open(schema.root);
    let pk = schema.primary_key();

    // Rows without a key get the next id after the largest one so far.
    let mut next_id: i128 = match tree.last(&mut db.pager)? {
        Some((key, _)) => i128::from(decode_key(&key)?) + 1,
        None => 1,
    };
    let mut ids = HashSet::new();
    let mut prepared = Vec::with_capacity(rows.len());
    for exprs in rows {
        if exprs.len() != targets.len() {
            return query(format!(
                "INSERT has {} values for {} columns",
                exprs.len(),
                targets.len()
            ));
        }
        let mut row = vec![Value::Null; schema.columns.len()];
        for (expr, &col) in exprs.iter().zip(&targets) {
            check_columns(expr, None)?;
            row[col] = fit(&schema, col, eval(expr, &Scope::empty())?)?;
        }
        let id = match pk.map(|p| &row[p]) {
            Some(Value::Int(id)) => *id,
            _ => {
                let id =
                    i64::try_from(next_id).map_err(|_| Error::Query("no row ids left".into()))?;
                if let Some(p) = pk {
                    row[p] = Value::Int(id);
                }
                id
            }
        };
        next_id = next_id.max(i128::from(id) + 1);
        let key = encode_key(id);
        if !ids.insert(id) || tree.get(&mut db.pager, &key)?.is_some() {
            return Err(duplicate_key(&schema, id));
        }
        prepared.push((key, row_bytes(&row)?));
    }

    for (key, bytes) in &prepared {
        tree.insert(&mut db.pager, key, bytes)?;
    }
    Ok(Output::Inserted(prepared.len()))
}

pub(super) fn update(
    db: &mut Database,
    table: &str,
    assignments: &[(String, Expr)],
    filter: Option<&Expr>,
) -> Result<Output> {
    let schema = db.table(table)?;
    let scope_schema = Some(&schema);
    let mut targets = Vec::new();
    for (name, expr) in assignments {
        let Some(i) = schema.column_index(name) else {
            return query(format!("no such column: {name}"));
        };
        check_columns(expr, scope_schema)?;
        targets.push((i, expr));
    }
    if let Some(filter) = filter {
        check_columns(filter, scope_schema)?;
    }
    let tree = BTree::open(schema.root);
    let pk = schema.primary_key();

    // Pass 1: the new version of every matching row, as (old key, new key, bytes).
    let mut changes = Vec::new();
    for (key, row) in scan(&mut db.pager, &schema)? {
        let scope = Scope {
            schema: scope_schema,
            row: &row,
        };
        if let Some(filter) = filter
            && !is_true(&eval(filter, &scope)?, "WHERE")?
        {
            continue;
        }
        // SET expressions all see the old row, so `SET a = b, b = a` swaps.
        let mut new_row = row.clone();
        for (col, expr) in &targets {
            new_row[*col] = fit(&schema, *col, eval(expr, &scope)?)?;
        }
        let new_key = match pk {
            None => key.clone(),
            Some(p) => match &new_row[p] {
                Value::Int(id) => encode_key(*id).to_vec(),
                _ => {
                    return query(format!(
                        "primary key {} can't be NULL",
                        schema.columns[p].name
                    ));
                }
            },
        };
        let bytes = row_bytes(&new_row)?;
        changes.push((key, new_key, bytes));
    }

    // A row whose key changes may land on a key that is free, or on one that
    // another changed row is moving away from, but not on a row that stays.
    let moving_from: HashSet<&[u8]> = changes
        .iter()
        .filter(|(old, new, _)| old != new)
        .map(|(old, _, _)| old.as_slice())
        .collect();
    let mut final_keys = HashSet::new();
    for (old, new, _) in &changes {
        let taken = !final_keys.insert(new.as_slice())
            || (old != new
                && !moving_from.contains(new.as_slice())
                && tree.get(&mut db.pager, new)?.is_some());
        if taken {
            return Err(duplicate_key(&schema, decode_key(new)?));
        }
    }

    // Pass 2: take moved rows out of their old places, then write every new version.
    for (old, new, _) in &changes {
        if old != new {
            tree.delete(&mut db.pager, old)?;
        }
    }
    for (_, new, bytes) in &changes {
        tree.insert(&mut db.pager, new, bytes)?;
    }
    Ok(Output::Updated(changes.len()))
}

pub(super) fn delete(db: &mut Database, table: &str, filter: Option<&Expr>) -> Result<Output> {
    let schema = db.table(table)?;
    if let Some(filter) = filter {
        check_columns(filter, Some(&schema))?;
    }
    let mut doomed = Vec::new();
    for (key, row) in scan(&mut db.pager, &schema)? {
        let scope = Scope {
            schema: Some(&schema),
            row: &row,
        };
        if let Some(filter) = filter
            && !is_true(&eval(filter, &scope)?, "WHERE")?
        {
            continue;
        }
        doomed.push(key);
    }
    let tree = BTree::open(schema.root);
    for key in &doomed {
        tree.delete(&mut db.pager, key)?;
    }
    Ok(Output::Deleted(doomed.len()))
}

/// `value` converted for column `col`, or an error naming the column.
fn fit(schema: &TableSchema, col: usize, value: Value) -> Result<Value> {
    let column = &schema.columns[col];
    let shown = format!("{} {}", value.type_name(), value.to_sql());
    match value.coerce(column.data_type) {
        Some(v) => Ok(v),
        None => query(format!(
            "column {} is {}, got {shown}",
            column.name, column.data_type
        )),
    }
}

fn row_bytes(row: &[Value]) -> Result<Vec<u8>> {
    let bytes = encode_row(row);
    if bytes.len() > MAX_ROW {
        return query(format!(
            "row too large: {} bytes (max {MAX_ROW})",
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn duplicate_key(schema: &TableSchema, id: i64) -> Error {
    let column = schema
        .primary_key()
        .map_or("rowid", |p| &schema.columns[p].name);
    Error::Query(format!(
        "duplicate primary key: {}.{column} = {id}",
        schema.name
    ))
}

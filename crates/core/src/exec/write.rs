//! INSERT, UPDATE and DELETE.
//!
//! Each works in two passes: first compute and check every row and index key
//! it will write (types, primary keys, sizes), then write. If the first pass
//! finds a problem, nothing has been written yet.

use std::collections::HashSet;

use super::access::{check_filter, fetch, plan, trace_plan, write_steps};
use super::eval::{Scope, check_columns, eval, is_true};
use super::index::{self, Indexed};
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
    let indexes = index::open_all(db, &schema)?;
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
        let entries: Vec<Vec<u8>> = indexes
            .iter()
            .map(|index| index.key(&row, id))
            .collect::<Result<_>>()?;
        prepared.push((key, row_bytes(&row)?, entries));
    }

    for (key, bytes, entries) in &prepared {
        tree.insert(&mut db.pager, key, bytes)?;
        for (index, entry) in indexes.iter().zip(entries) {
            index.tree.insert(&mut db.pager, entry, &[])?;
        }
    }
    Ok(Output::Inserted(prepared.len()))
}

/// Check an UPDATE's names. Returns the table and the column each SET fills.
pub(super) fn check_update<'a>(
    db: &mut Database,
    table: &str,
    assignments: &'a [(String, Expr)],
    filter: Option<&Expr>,
) -> Result<(TableSchema, Vec<(usize, &'a Expr)>)> {
    let schema = db.table(table)?;
    let mut targets = Vec::new();
    for (name, expr) in assignments {
        let Some(i) = schema.column_index(name) else {
            return query(format!("no such column: {name}"));
        };
        check_columns(expr, Some(&schema))?;
        targets.push((i, expr));
    }
    check_filter(filter, &schema)?;
    Ok((schema, targets))
}

/// An index entry an UPDATE replaces.
struct EntryChange {
    index: usize,
    old: Vec<u8>,
    new: Vec<u8>,
}

pub(super) fn update(
    db: &mut Database,
    table: &str,
    assignments: &[(String, Expr)],
    filter: Option<&Expr>,
) -> Result<Output> {
    let (schema, targets) = check_update(db, table, assignments, filter)?;
    let scope_schema = Some(&schema);
    let tree = BTree::open(schema.root);
    let indexes = index::open_all(db, &schema)?;
    let pk = schema.primary_key();
    let access = plan(db, &schema, filter)?;
    let action = format!("UPDATE {table}");
    trace_plan(db, write_steps(table, &access, filter, action));

    // Pass 1: the new version of every matching row, as (old key, new key, bytes).
    let mut changes = Vec::new();
    let mut entry_changes = Vec::new();
    for (key, row) in fetch(&mut db.pager, &schema, &access)? {
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
        for (i, index) in indexes.iter().enumerate() {
            let old = index.key(&row, decode_key(&key)?)?;
            let new = index.key(&new_row, decode_key(&new_key)?)?;
            if old != new {
                entry_changes.push(EntryChange { index: i, old, new });
            }
        }
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
    // Old entries go first too: one row's new entry may be another's old one.
    for change in &entry_changes {
        let index = &indexes[change.index];
        index.tree.delete(&mut db.pager, &change.old)?;
    }
    for change in &entry_changes {
        let index = &indexes[change.index];
        index.tree.insert(&mut db.pager, &change.new, &[])?;
    }
    Ok(Output::Updated(changes.len()))
}

/// Check a DELETE's names. Returns the table.
pub(super) fn check_delete(
    db: &mut Database,
    table: &str,
    filter: Option<&Expr>,
) -> Result<TableSchema> {
    let schema = db.table(table)?;
    check_filter(filter, &schema)?;
    Ok(schema)
}

pub(super) fn delete(db: &mut Database, table: &str, filter: Option<&Expr>) -> Result<Output> {
    let schema = check_delete(db, table, filter)?;
    let indexes: Vec<Indexed> = index::open_all(db, &schema)?;
    let access = plan(db, &schema, filter)?;
    let action = format!("DELETE FROM {table}");
    trace_plan(db, write_steps(table, &access, filter, action));

    let mut doomed = Vec::new();
    let mut entries = Vec::new();
    for (key, row) in fetch(&mut db.pager, &schema, &access)? {
        let scope = Scope {
            schema: Some(&schema),
            row: &row,
        };
        if let Some(filter) = filter
            && !is_true(&eval(filter, &scope)?, "WHERE")?
        {
            continue;
        }
        for (i, index) in indexes.iter().enumerate() {
            entries.push((i, index.key(&row, decode_key(&key)?)?));
        }
        doomed.push(key);
    }
    let tree = BTree::open(schema.root);
    for key in &doomed {
        tree.delete(&mut db.pager, key)?;
    }
    for (i, entry) in &entries {
        indexes[*i].tree.delete(&mut db.pager, entry)?;
    }
    Ok(Output::Deleted(doomed.len()))
}

/// `value` converted for column `col`, or an error naming the column.
fn fit(schema: &TableSchema, col: usize, value: Value) -> Result<Value> {
    let column = &schema.columns[col];
    let shown = format!("{} {}", value.type_name(), value.to_sql());
    match value.coerce(column.data_type) {
        // NaN compares equal to everything, so an index couldn't place it.
        Some(Value::Real(r)) if r.is_nan() => {
            query(format!("column {} can't hold NaN", column.name))
        }
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

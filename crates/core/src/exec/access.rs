//! Turning a plan into rows, and `EXPLAIN`.

use std::ops::Bound;

use super::eval::check_columns;
use super::select::{prepare, scan, select_steps};
use super::write::{check_delete, check_update};
use super::{Engine, Output};
use crate::btree::{BTree, Range};
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::mvcc::{TxnId, decode, unstamped};
use crate::pager::Pager;
use crate::planner::{Access, KeyRange, choose};
use crate::sql::{Expr, Statement};
use crate::trace::Event;
use crate::types::{Value, rowid_of};

/// A current row, as its table tree holds it.
pub(super) struct Stored {
    pub key: Vec<u8>,
    /// The transaction that wrote this version.
    pub created: TxnId,
    pub row: Vec<Value>,
}

impl Stored {
    pub fn decode(table: &TableSchema, key: Vec<u8>, value: &[u8]) -> Result<Stored> {
        let (created, row) = unstamped(value)?;
        Ok(Stored {
            key,
            created,
            row: decode(table, row)?,
        })
    }
}

/// The best way to find the rows of `table` that `filter` wants.
pub(super) fn plan(db: &mut Engine, table: &TableSchema, filter: Option<&Expr>) -> Result<Access> {
    let indexes = db.catalog.indexes_of(&mut db.pager, &table.name)?;
    Ok(choose(table, &indexes, filter))
}

/// Tell the tracer what is about to run.
pub(super) fn trace_plan(db: &Engine, steps: Vec<String>) {
    db.pager.tracer().event(&Event::Plan { steps });
}

fn range_entries<'p>(pager: &'p mut Pager, tree: &BTree, range: &KeyRange) -> Result<Range<'p>> {
    let start = range.start.as_ref().map(Vec::as_slice);
    let end = range.end.as_ref().map(Vec::as_slice);
    tree.range(pager, start, end)
}

/// The table keys an access path covers. Past rows aren't in any index, so
/// only the table's own key is any use when reading them.
pub(super) fn row_range(access: &Access) -> KeyRange {
    match access {
        Access::PrimaryKey { range, .. } => range.clone(),
        Access::FullScan | Access::Index { .. } => KeyRange {
            start: Bound::Unbounded,
            end: Bound::Unbounded,
        },
    }
}

/// The rows `access` finds, with their keys, in table order whichever way they
/// were found, so an index never changes the order of a result.
pub(super) fn fetch(
    pager: &mut Pager,
    table: &TableSchema,
    access: &Access,
) -> Result<Vec<Stored>> {
    let rows = BTree::open(table.root);
    match access {
        Access::FullScan => scan(pager, table),
        Access::PrimaryKey { range, .. } => {
            let mut found = Vec::new();
            for entry in range_entries(pager, &rows, range)? {
                let (key, bytes) = entry?;
                found.push(Stored::decode(table, key, &bytes)?);
            }
            Ok(found)
        }
        Access::Index { index, range, .. } => {
            let mut keys = Vec::new();
            for entry in range_entries(pager, &BTree::open(index.root), range)? {
                keys.push(rowid_of(&entry?.0)?);
            }
            keys.sort_unstable();
            let mut found = Vec::with_capacity(keys.len());
            for key in keys {
                let Some(bytes) = rows.get(pager, &key)? else {
                    return Err(Error::Corrupt(format!(
                        "index {} points to a row that isn't there",
                        index.name
                    )));
                };
                found.push(Stored::decode(table, key.to_vec(), &bytes)?);
            }
            Ok(found)
        }
    }
}

/// The steps of a statement that writes the rows it finds.
pub(super) fn write_steps(
    table: &str,
    access: &Access,
    filter: Option<&Expr>,
    action: String,
) -> Vec<String> {
    let mut steps = vec![access.describe(table)];
    if let Some(filter) = filter {
        steps.push(format!("FILTER {filter}"));
    }
    steps.push(action);
    steps
}

/// `EXPLAIN`: check the statement as if to run it, and show its steps.
pub(super) fn explain(db: &mut Engine, statement: &Statement) -> Result<Output> {
    let steps = match statement {
        Statement::Select(select) => {
            let prepared = prepare(db, select)?;
            select_steps(select, &prepared)
        }
        Statement::Update {
            table,
            assignments,
            filter,
        } => {
            let (schema, _) = check_update(db, table, assignments, filter.as_ref())?;
            let access = plan(db, &schema, filter.as_ref())?;
            write_steps(table, &access, filter.as_ref(), format!("UPDATE {table}"))
        }
        Statement::Delete { table, filter } => {
            let schema = check_delete(db, table, filter.as_ref())?;
            let access = plan(db, &schema, filter.as_ref())?;
            let action = format!("DELETE FROM {table}");
            write_steps(table, &access, filter.as_ref(), action)
        }
        other => return Err(Error::Query(format!("nothing to explain in {other}"))),
    };
    Ok(Output::Rows {
        columns: vec!["plan".into()],
        rows: steps
            .into_iter()
            .map(|step| vec![Value::Text(step)])
            .collect(),
    })
}

/// Check `filter` against `table`, if there is one.
pub(super) fn check_filter(filter: Option<&Expr>, table: &TableSchema) -> Result<()> {
    match filter {
        Some(filter) => check_columns(filter, Some(table)),
        None => Ok(()),
    }
}

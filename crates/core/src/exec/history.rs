//! `SELECT ... HISTORY`, and `VACUUM HISTORY`.
//!
//! `HISTORY` lists a table's versions as rows of a slightly wider table, so
//! `WHERE`, `ORDER BY` and the rest work on them with no special handling.

use std::collections::HashMap;

use super::{Engine, Output, plural};
use crate::btree::BTree;
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::mvcc::{TxnId, unstamped, versions};
use crate::pager::Pager;
use crate::planner::KeyRange;
use crate::sql::ColumnDef;
use crate::time::format_timestamp;
use crate::trace::Event;
use crate::types::{DataType, Value};

/// The columns `HISTORY` adds after the table's own.
const EXTRA_COLUMNS: [(&str, DataType); 4] = [
    ("created_txn", DataType::Int),
    ("created_at", DataType::Text),
    ("deleted_txn", DataType::Int),
    ("deleted_at", DataType::Text),
];

/// `table` with the extra columns. A column already using one of those names
/// would be ambiguous, so it is refused.
pub(super) fn schema(table: &TableSchema) -> Result<TableSchema> {
    let mut wider = table.clone();
    for (name, data_type) in EXTRA_COLUMNS {
        if table.column_index(name).is_some() {
            return Err(Error::Query(format!(
                "HISTORY can't show {}, because column {name} clashes with its own",
                table.name
            )));
        }
        wider.columns.push(ColumnDef {
            name: name.into(),
            data_type,
            primary_key: false,
        });
    }
    Ok(wider)
}

/// Every version of the rows in `range`, by row, oldest first, each followed
/// by the extra columns. The open transaction has no commit time yet, so its
/// stamps show a NULL time.
pub(super) fn rows(
    db: &mut Engine,
    table: &TableSchema,
    range: &KeyRange,
) -> Result<Vec<Vec<Value>>> {
    let mut times: HashMap<TxnId, Value> = HashMap::new();
    let mut time_of = |db: &mut Engine, txn: TxnId| -> Result<Value> {
        if let Some(time) = times.get(&txn) {
            return Ok(time.clone());
        }
        let time = match db.commits.time_of(&mut db.pager, txn)? {
            Some(time) => Value::Text(format_timestamp(time)),
            None => Value::Null,
        };
        times.insert(txn, time.clone());
        Ok(time)
    };
    let mut rows = Vec::new();
    for version in versions(&mut db.pager, table, range)? {
        let mut row = version.row;
        row.push(Value::Int(version.created as i64));
        row.push(time_of(db, version.created)?);
        match version.deleted {
            Some(deleted) => {
                row.push(Value::Int(deleted as i64));
                row.push(time_of(db, deleted)?);
            }
            None => row.extend([Value::Null, Value::Null]),
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Delete `table`'s versions replaced by `txn` or earlier. Returns how many.
fn trim(pager: &mut Pager, table: &TableSchema, txn: TxnId) -> Result<usize> {
    let history = BTree::open(table.history_root);
    let mut gone = Vec::new();
    for entry in history.iter(pager)? {
        let (key, value) = entry?;
        if unstamped(&value)?.0 <= txn {
            gone.push(key);
        }
    }
    for key in &gone {
        history.delete(pager, key)?;
    }
    Ok(gone.len())
}

/// `VACUUM HISTORY BEFORE <time>`. The moment at `<time>` stays readable, so
/// what goes is whatever only earlier moments could see. It rewrites nothing
/// that any query can tell apart, so it takes no transaction id.
pub(super) fn vacuum(db: &mut Engine, before: i64) -> Result<Output> {
    if db.in_transaction() {
        return Err(Error::Query("can't vacuum inside a transaction".into()));
    }
    let txn = db.commits.txn_at(&mut db.pager, before, db.last.txn)?;
    if let Some(&oldest) = db.pins.keys().next()
        && oldest < txn
    {
        return Err(Error::Query(format!(
            "a reader is still looking at txn {oldest}"
        )));
    }
    let mut removed = 0;
    for table in db.tables()? {
        removed += trim(&mut db.pager, &table, txn)?;
    }
    let mut tables = 0;
    for table in db.catalog.dropped(&mut db.pager)? {
        if table.dropped <= txn {
            db.catalog.remove_dropped(&mut db.pager, &table)?;
            tables += 1;
        } else {
            removed += trim(&mut db.pager, &table.schema, txn)?;
        }
    }
    let floor = db.pager.history_floor().max(txn);
    db.pager.set_history_floor(floor);
    db.pager.tracer().event(&Event::Vacuumed {
        txn: floor,
        versions: removed,
        tables,
    });
    Ok(Output::Message(format!(
        "removed {} and {}",
        plural(removed, "old version"),
        plural(tables, "dropped table")
    )))
}

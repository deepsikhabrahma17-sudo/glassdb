//! Indexes: creating and dropping them, keeping them in step, and checking them.
//!
//! An index is a B+ tree with an entry for every row of its table. An entry's
//! key is the row's indexed columns then its row id (see `types::index_key`),
//! and its value is empty. Like a table's rows, its keys are worked out in a
//! statement's checking pass, so a value too long to index changes nothing.

use super::select::scan;
use super::{Engine, Output};
use crate::btree::{BTree, MAX_KEY};
use crate::catalog::{IndexSchema, TableSchema};
use crate::error::{Error, Result};
use crate::sql::CreateIndex;
use crate::types::{Value, decode_key, index_key};

/// An index opened for writing.
pub(super) struct Indexed {
    name: String,
    pub tree: BTree,
    /// Which table columns the key is made of, in order.
    columns: Vec<usize>,
}

impl Indexed {
    fn open(index: &IndexSchema, table: &TableSchema) -> Result<Indexed> {
        let columns = index
            .columns
            .iter()
            .map(|name| {
                table.column_index(name).ok_or_else(|| {
                    Error::Corrupt(format!("index {} names column {name}", index.name))
                })
            })
            .collect::<Result<_>>()?;
        Ok(Indexed {
            name: index.name.clone(),
            tree: BTree::open(index.root),
            columns,
        })
    }

    /// The entry key of `row`, or an error if it would be too long.
    pub fn key(&self, row: &[Value], rowid: i64) -> Result<Vec<u8>> {
        let values: Vec<Value> = self.columns.iter().map(|&c| row[c].clone()).collect();
        let key = index_key(&values, rowid);
        if key.len() > MAX_KEY {
            return Err(Error::Query(format!(
                "value too long for index {}: key is {} bytes (max {MAX_KEY})",
                self.name,
                key.len()
            )));
        }
        Ok(key)
    }
}

/// The indexes on `table`, ready to write to.
pub(super) fn open_all(db: &mut Engine, table: &TableSchema) -> Result<Vec<Indexed>> {
    let indexes = db.catalog.indexes_of(&mut db.pager, &table.name)?;
    indexes.iter().map(|i| Indexed::open(i, table)).collect()
}

pub(super) fn create(db: &mut Engine, def: &CreateIndex) -> Result<Output> {
    let name = &def.name;
    if def.if_not_exists && db.catalog.get_index(&mut db.pager, name)?.is_some() {
        return Ok(Output::Message(format!(
            "index {name} already exists, skipped"
        )));
    }
    let table = db.table(&def.table)?;
    db.catalog.check_new_index(&mut db.pager, def, &table)?;

    // Work out every key before writing anything. Sorted keys also go into
    // the tree in order, touching each leaf once instead of hopping around.
    let index = IndexSchema {
        name: name.clone(),
        table: table.name.clone(),
        root: 0,
        columns: def.columns.clone(),
    };
    let keys = keys_of(&Indexed::open(&index, &table)?, &table, db)?;

    let txn = db.next_txn();
    let index = db.catalog.create_index(&mut db.pager, def, txn)?;
    db.wrote = true;
    let tree = BTree::open(index.root);
    for key in &keys {
        tree.insert(&mut db.pager, key, &[])?;
    }
    Ok(Output::Message(format!("created index {name}")))
}

/// Every key `index` should hold for `table`, sorted.
fn keys_of(index: &Indexed, table: &TableSchema, db: &mut Engine) -> Result<Vec<Vec<u8>>> {
    let mut keys = Vec::new();
    for stored in scan(&mut db.pager, table)? {
        keys.push(index.key(&stored.row, decode_key(&stored.key)?)?);
    }
    keys.sort_unstable();
    Ok(keys)
}

pub(super) fn drop(db: &mut Engine, name: &str, if_exists: bool) -> Result<Output> {
    match db.catalog.drop_index(&mut db.pager, name)? {
        Some(_) => {
            db.wrote = true;
            Ok(Output::Message(format!("dropped index {name}")))
        }
        None if if_exists => Ok(Output::Message(format!("no index {name}, skipped"))),
        None => Err(Error::Query(format!("no such index: {name}"))),
    }
}

/// Verify every index on `table`: its tree's invariants, and that it holds
/// exactly one entry per row. Returns how many pages the indexes use, and
/// how many indexes there were.
pub(super) fn check(db: &mut Engine, table: &TableSchema) -> Result<(usize, usize)> {
    let indexes = db.catalog.indexes_of(&mut db.pager, &table.name)?;
    let mut pages = 0;
    for index in &indexes {
        let stats = BTree::open(index.root).check(&mut db.pager)?;
        pages += stats.internal_nodes + stats.leaves;

        let expected = keys_of(&Indexed::open(index, table)?, table, db)?;
        let mut found = Vec::with_capacity(expected.len());
        for entry in BTree::open(index.root).iter(&mut db.pager)? {
            found.push(entry?.0);
        }
        if found != expected {
            return Err(Error::Corrupt(format!(
                "index {} holds {} entries that don't match the {} rows of {}",
                index.name,
                found.len(),
                expected.len(),
                table.name
            )));
        }
    }
    Ok((pages, indexes.len()))
}

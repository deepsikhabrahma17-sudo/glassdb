//! Dropped tables, which stay in the catalog so the past can still be read.
//!
//! `DROP TABLE` frees the table's indexes, but its rows and history stay until
//! `VACUUM HISTORY` passes the drop. Its entry moves to the key `name`, a zero
//! byte, then the dropping transaction (big-endian). No name holds a zero
//! byte, so these keys can't clash with a live entry, and one name can have
//! any number of them.

use std::ops::Bound;

use super::{Catalog, Entry, TableSchema, decode_entry, encode_entry};
use crate::btree::BTree;
use crate::error::Result;
use crate::mvcc::{TxnId, visible};
use crate::pager::Pager;

/// A dropped table's key is this much longer than its name.
pub(super) const DROPPED_KEY_EXTRA: usize = 9;

/// A dropped table whose rows are still kept.
#[derive(Debug, Clone, PartialEq)]
pub struct DroppedTable {
    pub schema: TableSchema,
    /// The transaction that dropped it.
    pub dropped: TxnId,
}

fn dropped_key(name: &str, dropped: TxnId) -> Vec<u8> {
    let mut key = name.as_bytes().to_vec();
    key.push(0);
    key.extend(dropped.to_be_bytes());
    key
}

pub(super) fn is_dropped_key(key: &[u8]) -> bool {
    key.contains(&0)
}

pub(super) fn name_of_dropped_key(key: &[u8]) -> String {
    let end = key.len().saturating_sub(DROPPED_KEY_EXTRA);
    String::from_utf8_lossy(&key[..end]).into_owned()
}

impl Catalog {
    /// Remove a table. Its indexes go at once, but its rows wait for vacuum,
    /// unless transaction `txn` also created it: then no moment ever saw it,
    /// so nothing needs keeping. Returns the table, if it existed.
    pub fn drop(&self, pager: &mut Pager, name: &str, txn: TxnId) -> Result<Option<TableSchema>> {
        let Some(schema) = self.get(pager, name)? else {
            return Ok(None);
        };
        for index in self.indexes_of(pager, name)? {
            self.drop_index(pager, &index.name)?;
        }
        self.tree.delete(pager, name.as_bytes())?;
        if schema.created == txn {
            self.destroy(pager, &schema)?;
        } else {
            let sql = schema.create_sql();
            let entry = encode_entry(schema.root, schema.history_root, schema.created, txn, &sql);
            self.tree.insert(pager, &dropped_key(name, txn), &entry)?;
        }
        Ok(Some(schema))
    }

    fn destroy(&self, pager: &mut Pager, schema: &TableSchema) -> Result<()> {
        BTree::open(schema.root).destroy(pager)?;
        BTree::open(schema.history_root).destroy(pager)
    }

    /// Every dropped table still kept, sorted by name, then oldest first.
    pub fn dropped(&self, pager: &mut Pager) -> Result<Vec<DroppedTable>> {
        let entries = self.entries(pager)?.into_iter();
        Ok(entries
            .filter_map(|entry| match entry {
                Entry::Dropped(table) => Some(table),
                _ => None,
            })
            .collect())
    }

    /// Delete a dropped table's rows for good, and free its pages.
    pub fn remove_dropped(&self, pager: &mut Pager, table: &DroppedTable) -> Result<()> {
        self.destroy(pager, &table.schema)?;
        let key = dropped_key(&table.schema.name, table.dropped);
        self.tree.delete(pager, &key)?;
        Ok(())
    }

    /// The table called `name` at transaction `at`: the live one if it existed
    /// by then, or else a dropped one that hadn't been dropped yet.
    pub fn table_at(
        &self,
        pager: &mut Pager,
        name: &str,
        at: TxnId,
    ) -> Result<Option<TableSchema>> {
        if let Some(table) = self.get(pager, name)?
            && visible(table.created, None, at)
        {
            return Ok(Some(table));
        }
        let mut start = name.as_bytes().to_vec();
        start.push(0);
        let mut end = name.as_bytes().to_vec();
        end.push(1);
        let range = (
            Bound::Included(start.as_slice()),
            Bound::Excluded(end.as_slice()),
        );
        for entry in self.tree.range(pager, range.0, range.1)? {
            let (_, value) = entry?;
            if let Entry::Dropped(table) = decode_entry(name, &value)?
                && visible(table.schema.created, Some(table.dropped), at)
            {
                return Ok(Some(table.schema));
            }
        }
        Ok(None)
    }
}

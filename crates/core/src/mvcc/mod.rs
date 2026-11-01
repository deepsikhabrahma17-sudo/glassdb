//! Row versions: how a table remembers its past.
//!
//! A write never loses the row it replaces. Each table has two B+ trees:
//!
//! ```text
//! table tree     rowid                -> created_by, row     (the current rows)
//! history tree   rowid + created_by   -> deleted_by, row     (replaced versions)
//! ```
//!
//! An UPDATE or DELETE moves the old version into the history tree, stamped
//! with the transaction that replaced it. Ordinary queries and indexes only
//! ever see the table tree, so history doesn't slow them down.
//!
//! A version is visible at transaction `n` if it was created by then and not
//! yet deleted by then (`visible`). `AS OF` and readers both work this way,
//! and that test is also what hides a transaction still in progress: its id
//! is newer than any moment a reader can be looking at.

pub mod commits;

#[cfg(test)]
mod tests;

use std::ops::Bound;

use crate::btree::BTree;
use crate::catalog::TableSchema;
use crate::error::{Error, Result};
use crate::pager::Pager;
use crate::planner::KeyRange;
use crate::trace::Event;
use crate::types::{Value, decode_key, decode_row, encode_key};

pub use commits::{COMMITS_ROOT, Commit, CommitLog};

/// A transaction's number. 0 means the empty database, before any commit.
pub type TxnId = u64;

/// Bytes a transaction id takes in a stored value.
pub const TXN_BYTES: usize = 8;

/// `row`, stamped with a transaction: `created_by` in the table tree, and
/// `deleted_by` in the history tree.
pub fn stamped(txn: TxnId, row: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(TXN_BYTES + row.len());
    value.extend(txn.to_le_bytes());
    value.extend(row);
    value
}

/// A stored value split into its transaction and its row.
pub fn unstamped(value: &[u8]) -> Result<(TxnId, &[u8])> {
    match value.split_first_chunk::<TXN_BYTES>() {
        Some((txn, row)) => Ok((TxnId::from_le_bytes(*txn), row)),
        None => Err(Error::Corrupt("a stored row has no version stamp".into())),
    }
}

/// A history tree key: every version of a row sits together, oldest first.
pub fn history_key(rowid: i64, created: TxnId) -> Vec<u8> {
    let mut key = encode_key(rowid).to_vec();
    key.extend(created.to_be_bytes());
    key
}

pub fn split_history_key(key: &[u8]) -> Result<(i64, TxnId)> {
    let parts = key.split_first_chunk::<8>();
    match parts.and_then(|(id, txn)| Some((id, <[u8; 8]>::try_from(txn).ok()?))) {
        Some((id, txn)) => Ok((decode_key(id)?, TxnId::from_be_bytes(txn))),
        None => Err(Error::Corrupt("a history key has the wrong size".into())),
    }
}

/// Whether a version created by `created` and deleted by `deleted` existed
/// at transaction `at`.
pub fn visible(created: TxnId, deleted: Option<TxnId>, at: TxnId) -> bool {
    created <= at && deleted.is_none_or(|deleted| deleted > at)
}

/// The history keys of every row in a range of table keys. A table key is the
/// first 8 bytes of a history key, so a bound only has to reach past the
/// row's last possible version, not stop at its first.
pub fn history_bounds(range: &KeyRange) -> KeyRange {
    let after_every_version = |key: &Vec<u8>| {
        let mut key = key.clone();
        key.extend([0xFF; TXN_BYTES]);
        key
    };
    KeyRange {
        start: match &range.start {
            Bound::Excluded(key) => Bound::Excluded(after_every_version(key)),
            other => other.clone(),
        },
        end: match &range.end {
            Bound::Included(key) => Bound::Included(after_every_version(key)),
            other => other.clone(),
        },
    }
}

/// One version of a row.
#[derive(Debug, Clone, PartialEq)]
pub struct Version {
    pub rowid: i64,
    pub created: TxnId,
    /// `None` for the current version.
    pub deleted: Option<TxnId>,
    pub row: Vec<Value>,
}

/// A row of `table`, from its stored bytes.
pub(crate) fn decode(table: &TableSchema, bytes: &[u8]) -> Result<Vec<Value>> {
    let row = decode_row(bytes)?;
    if row.len() != table.columns.len() {
        return Err(Error::Corrupt(format!(
            "a row of {} has {} values for {} columns",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    Ok(row)
}

fn current_version(table: &TableSchema, key: &[u8], value: &[u8]) -> Result<Version> {
    let (created, row) = unstamped(value)?;
    Ok(Version {
        rowid: decode_key(key)?,
        created,
        deleted: None,
        row: decode(table, row)?,
    })
}

fn old_version(table: &TableSchema, key: &[u8], value: &[u8]) -> Result<Version> {
    let (rowid, created) = split_history_key(key)?;
    let (deleted, row) = unstamped(value)?;
    Ok(Version {
        rowid,
        created,
        deleted: Some(deleted),
        row: decode(table, row)?,
    })
}

fn as_slices(range: &KeyRange) -> (Bound<&[u8]>, Bound<&[u8]>) {
    (
        range.start.as_ref().map(Vec::as_slice),
        range.end.as_ref().map(Vec::as_slice),
    )
}

/// Every version of the rows in a range of table keys, by row, oldest first.
pub fn versions(pager: &mut Pager, table: &TableSchema, range: &KeyRange) -> Result<Vec<Version>> {
    let mut found = Vec::new();
    let (start, end) = as_slices(range);
    for entry in BTree::open(table.root).range(pager, start, end)? {
        let (key, value) = entry?;
        found.push(current_version(table, &key, &value)?);
    }
    let bounds = history_bounds(range);
    let (start, end) = as_slices(&bounds);
    for entry in BTree::open(table.history_root).range(pager, start, end)? {
        let (key, value) = entry?;
        found.push(old_version(table, &key, &value)?);
    }
    found.sort_unstable_by_key(|version| (version.rowid, version.created));
    Ok(found)
}

/// The rows in `range` as they stood at transaction `at`, in table order.
pub fn rows_at(
    pager: &mut Pager,
    table: &TableSchema,
    range: &KeyRange,
    at: TxnId,
) -> Result<Vec<Vec<Value>>> {
    let all = versions(pager, table, range)?;
    let total = all.len();
    let rows: Vec<Vec<Value>> = all
        .into_iter()
        .filter(|version| visible(version.created, version.deleted, at))
        .map(|version| version.row)
        .collect();
    pager.tracer().event(&Event::PastRead {
        txn: at,
        visible: rows.len(),
        hidden: total - rows.len(),
    });
    Ok(rows)
}

/// What `check` found in one table's versions.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct VersionStats {
    /// Pages in the history tree.
    pub pages: usize,
    pub old_versions: usize,
}

/// Verify a table's versions: the history tree, that each stamp is in range,
/// and that a row's versions follow one another without overlapping. `newest`
/// is the newest id anything may carry, and `floor` is how far vacuum has
/// gone: every version it should have removed must be gone.
pub fn check(
    pager: &mut Pager,
    table: &TableSchema,
    newest: TxnId,
    floor: TxnId,
) -> Result<VersionStats> {
    let tree_stats = BTree::open(table.history_root).check(pager)?;
    let everything = KeyRange {
        start: Bound::Unbounded,
        end: Bound::Unbounded,
    };
    let all = versions(pager, table, &everything)?;
    let corrupt = |version: &Version, why: &str| {
        Err(Error::Corrupt(format!(
            "row {} of {} (created by txn {}): {why}",
            version.rowid, table.name, version.created
        )))
    };
    let mut old_versions = 0;
    for (i, version) in all.iter().enumerate() {
        if version.created < table.created.max(1) {
            return corrupt(version, "older than its table");
        }
        if version.created > newest {
            return corrupt(version, "from a transaction that hasn't happened");
        }
        if let Some(deleted) = version.deleted {
            old_versions += 1;
            if deleted <= version.created {
                return corrupt(version, "deleted before it was created");
            }
            if deleted > newest {
                return corrupt(version, "deleted by a transaction that hasn't happened");
            }
            if deleted <= floor {
                return corrupt(version, "should have been vacuumed");
            }
        } else if all
            .get(i + 1)
            .is_some_and(|next| next.rowid == version.rowid)
        {
            return corrupt(version, "a current version with a newer one after it");
        }
        if let Some(previous) = i.checked_sub(1).map(|p| &all[p])
            && previous.rowid == version.rowid
            && previous.deleted.is_some_and(|end| end > version.created)
        {
            return corrupt(version, "overlaps the version before it");
        }
    }
    Ok(VersionStats {
        pages: tree_stats.internal_nodes + tree_stats.leaves,
        old_versions,
    })
}

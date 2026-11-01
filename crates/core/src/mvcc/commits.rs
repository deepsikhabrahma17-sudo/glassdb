//! The commit log: which transactions committed, and when.
//!
//! A B+ tree mapping each transaction id (8 bytes, big-endian, so keys sort
//! by number) to its commit time (8 bytes, little-endian). Its root is always
//! page 2. An entry goes in as part of the transaction it records, so the two
//! reach the write-ahead log, and survive a crash, together.
//!
//! Ids count up from 1 with no gaps, and times never go backwards. Between
//! them those two facts let `txn_at` binary-search the log by time.

use super::TxnId;
use crate::btree::BTree;
use crate::error::{Error, Result};
use crate::pager::{PageId, Pager};

/// The commit log's root page, right after the catalog's.
pub const COMMITS_ROOT: PageId = 2;

/// One committed transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    pub txn: TxnId,
    /// Microseconds since the Unix epoch, UTC.
    pub time: i64,
}

pub struct CommitLog {
    tree: BTree,
}

fn key(txn: TxnId) -> [u8; 8] {
    txn.to_be_bytes()
}

fn decode(key: &[u8], value: &[u8]) -> Result<Commit> {
    match (<[u8; 8]>::try_from(key), <[u8; 8]>::try_from(value)) {
        (Ok(txn), Ok(time)) => Ok(Commit {
            txn: TxnId::from_be_bytes(txn),
            time: i64::from_le_bytes(time),
        }),
        _ => Err(Error::Corrupt(
            "a commit log entry has the wrong size".into(),
        )),
    }
}

impl CommitLog {
    /// Make the log in a brand-new database.
    pub fn create(pager: &mut Pager) -> Result<CommitLog> {
        let tree = BTree::create(pager)?;
        debug_assert_eq!(tree.root(), COMMITS_ROOT);
        Ok(CommitLog { tree })
    }

    pub fn open() -> CommitLog {
        CommitLog {
            tree: BTree::open(COMMITS_ROOT),
        }
    }

    /// The newest commit, if there has been one.
    pub fn last(&self, pager: &mut Pager) -> Result<Option<Commit>> {
        match self.tree.last(pager)? {
            Some((key, value)) => Ok(Some(decode(&key, &value)?)),
            None => Ok(None),
        }
    }

    pub fn record(&self, pager: &mut Pager, txn: TxnId, time: i64) -> Result<()> {
        self.tree.insert(pager, &key(txn), &time.to_le_bytes())?;
        Ok(())
    }

    /// When `txn` committed, or `None` if it hasn't.
    pub fn time_of(&self, pager: &mut Pager, txn: TxnId) -> Result<Option<i64>> {
        match self.tree.get(pager, &key(txn))? {
            Some(value) => Ok(Some(decode(&key(txn), &value)?.time)),
            None => Ok(None),
        }
    }

    /// The last transaction committed at or before `time`, or 0, the empty
    /// database, if none had. `latest` is the newest id in the log.
    pub fn txn_at(&self, pager: &mut Pager, time: i64, latest: TxnId) -> Result<TxnId> {
        // Invariant: `low` committed by `time`, and `high` did not.
        let (mut low, mut high) = (0, latest + 1);
        while high - low > 1 {
            let middle = low + (high - low) / 2;
            match self.time_of(pager, middle)? {
                Some(found) if found <= time => low = middle,
                Some(_) => high = middle,
                None => return Err(missing(middle)),
            }
        }
        Ok(low)
    }

    /// Up to `limit` commits, newest first.
    pub fn recent(&self, pager: &mut Pager, limit: usize) -> Result<Vec<Commit>> {
        let Some(last) = self.last(pager)? else {
            return Ok(Vec::new());
        };
        let mut commits = Vec::new();
        for txn in (1..=last.txn).rev().take(limit) {
            let time = self.time_of(pager, txn)?.ok_or_else(|| missing(txn))?;
            commits.push(Commit { txn, time });
        }
        Ok(commits)
    }

    /// Verify the tree, that it holds exactly ids 1 to `latest`, and that
    /// times never go backwards. Returns how many pages it uses.
    pub fn check(&self, pager: &mut Pager, latest: TxnId) -> Result<usize> {
        let stats = self.tree.check(pager)?;
        let mut expected = 1;
        let mut previous = i64::MIN;
        for entry in self.tree.iter(pager)? {
            let (key, value) = entry?;
            let commit = decode(&key, &value)?;
            if commit.txn != expected {
                return Err(Error::Corrupt(format!(
                    "the commit log has txn {} where txn {expected} should be",
                    commit.txn
                )));
            }
            if commit.time < previous {
                return Err(Error::Corrupt(format!(
                    "the commit time of txn {} goes backwards",
                    commit.txn
                )));
            }
            previous = commit.time;
            expected += 1;
        }
        if expected != latest + 1 {
            return Err(Error::Corrupt(format!(
                "the commit log ends at txn {} but should reach txn {latest}",
                expected - 1
            )));
        }
        Ok(stats.internal_nodes + stats.leaves)
    }
}

fn missing(txn: TxnId) -> Error {
    Error::Corrupt(format!("the commit log has no txn {txn}"))
}

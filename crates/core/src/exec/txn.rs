//! `BEGIN`, `COMMIT` and `ROLLBACK`, and what a failed statement does.
//!
//! Outside a transaction, each statement commits when it finishes. Inside
//! one, changes wait in the cache and the log until `COMMIT`, and `ROLLBACK`
//! throws them away. The catalog keeps no state of its own in memory, so
//! `CREATE TABLE` and `DROP TABLE` roll back like any other change.
//!
//! A transaction that changes something takes the next id when it commits,
//! and its row versions carry that id from the start. Ids are given out only
//! at commit, so a rollback leaves no gap: the versions stamped with its id
//! are gone, and the next transaction can use it.

use super::{Engine, Output};
use crate::error::{Error, Result};
use crate::mvcc::{Commit, TxnId};
use crate::sql::Statement;
use crate::trace::Event;

/// Where the database is in a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Txn {
    None,
    Open,
    /// A statement failed part-way, so half of it may be in the transaction.
    /// Only `ROLLBACK` can run until the transaction is gone.
    Failed,
}

/// Statements check everything before they write, so an error found by that
/// check has changed nothing. Any other error may have struck part-way.
fn changed_nothing(err: &Error) -> bool {
    matches!(err, Error::Query(_) | Error::Syntax { .. })
}

fn no_transaction<T>() -> Result<T> {
    Err(Error::Query("no transaction in progress".into()))
}

pub(super) fn failed<T>() -> Result<T> {
    Err(Error::Query(
        "this transaction failed: ROLLBACK to end it".into(),
    ))
}

impl Engine {
    /// Whether a transaction is waiting for `COMMIT` or `ROLLBACK`.
    pub fn in_transaction(&self) -> bool {
        self.txn != Txn::None
    }

    /// The id the transaction in progress will commit under.
    pub(super) fn next_txn(&self) -> TxnId {
        self.last.txn + 1
    }

    /// Commit whatever has changed. A transaction that changed rows or the
    /// schema goes in the commit log; times are held from going backwards,
    /// since a lookup by time relies on them being in order.
    fn commit_changes(&mut self) -> Result<()> {
        if !self.wrote {
            return self.pager.flush();
        }
        let commit = Commit {
            txn: self.next_txn(),
            time: self.clock.now().max(self.last.time),
        };
        self.commits
            .record(&mut self.pager, commit.txn, commit.time)?;
        self.pager.flush()?;
        self.last = commit;
        self.wrote = false;
        self.pager.tracer().event(&Event::Committed {
            txn: commit.txn,
            time: commit.time,
        });
        Ok(())
    }

    /// Run a statement other than `BEGIN`, `COMMIT` or `ROLLBACK`.
    pub(super) fn run_data_statement(&mut self, statement: &Statement) -> Result<Output> {
        match self.txn {
            Txn::Failed => failed(),
            Txn::Open => {
                let result = self.dispatch(statement);
                if result.as_ref().is_err_and(|err| !changed_nothing(err)) {
                    self.txn = Txn::Failed;
                }
                result
            }
            Txn::None if matches!(statement, Statement::Select(_) | Statement::Explain(_)) => {
                self.dispatch(statement)
            }
            Txn::None => {
                let result = self.dispatch(statement).and_then(|output| {
                    self.commit_changes()?;
                    Ok(output)
                });
                if let Err(err) = &result {
                    self.wrote = false;
                    if !changed_nothing(err) {
                        // Don't leave half a statement for the next commit to
                        // sweep up. The first error is the one worth reporting.
                        let _ = self.pager.rollback();
                    }
                }
                result
            }
        }
    }

    pub(super) fn begin(&mut self) -> Result<Output> {
        if self.in_transaction() {
            return Err(Error::Query("already in a transaction".into()));
        }
        self.txn = Txn::Open;
        self.pager.tracer().event(&Event::TxnBegin);
        Ok(Output::Message("transaction started".into()))
    }

    pub(super) fn commit(&mut self) -> Result<Output> {
        match self.txn {
            Txn::None => return no_transaction(),
            Txn::Failed => return failed(),
            Txn::Open => {}
        }
        if let Err(err) = self.commit_changes() {
            self.txn = Txn::Failed;
            return Err(err);
        }
        self.txn = Txn::None;
        self.pager.tracer().event(&Event::TxnCommit);
        Ok(Output::Message("committed".into()))
    }

    pub(super) fn rollback(&mut self) -> Result<Output> {
        if !self.in_transaction() {
            return no_transaction();
        }
        self.txn = Txn::None;
        self.wrote = false;
        self.pager.rollback()?;
        Ok(Output::Message("rolled back".into()))
    }
}

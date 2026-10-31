//! `BEGIN`, `COMMIT` and `ROLLBACK`, and what a failed statement does.
//!
//! Outside a transaction, each statement commits when it finishes. Inside
//! one, changes wait in the cache and the log until `COMMIT`, and `ROLLBACK`
//! throws them away. The catalog keeps no state of its own in memory, so
//! `CREATE TABLE` and `DROP TABLE` roll back like any other change.

use super::{Database, Output};
use crate::error::{Error, Result};
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

fn failed<T>() -> Result<T> {
    Err(Error::Query(
        "this transaction failed: ROLLBACK to end it".into(),
    ))
}

impl Database {
    /// Whether a transaction is waiting for `COMMIT` or `ROLLBACK`.
    pub fn in_transaction(&self) -> bool {
        self.txn != Txn::None
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
                    self.pager.flush()?;
                    Ok(output)
                });
                if result.as_ref().is_err_and(|err| !changed_nothing(err)) {
                    // Don't leave half a statement for the next commit to
                    // sweep up. The first error is the one worth reporting.
                    let _ = self.pager.rollback();
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
        if let Err(err) = self.pager.flush() {
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
        self.pager.rollback()?;
        Ok(Output::Message("rolled back".into()))
    }
}

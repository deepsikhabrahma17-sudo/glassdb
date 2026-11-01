//! The handles a program holds: one `Database` that writes, and any number of
//! `Reader`s, which other threads can use while it does.
//!
//! All of them share one `Engine` behind a mutex, held for one statement at a
//! time. So a reader can wait for a statement to finish, but never for a
//! transaction: between a writer's statements it reads straight past whatever
//! is uncommitted, because the version stamps hide it (see `mvcc`).

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use super::CheckReport;
use super::Output;
use super::engine::Engine;
use crate::catalog::{IndexSchema, TableSchema};
use crate::error::{Error, Result};
use crate::mvcc::{Commit, TxnId};
use crate::sql::{Statement, parse_one};
use crate::storage::Storage;
use crate::time::Clock;
use crate::trace::Tracer;

/// `None` once the database has closed.
type Shared = Arc<Mutex<Option<Engine>>>;

fn with<T>(shared: &Shared, work: impl FnOnce(&mut Engine) -> Result<T>) -> Result<T> {
    let mut engine = shared
        .lock()
        .map_err(|_| Error::Corrupt("a thread panicked while using the database".into()))?;
    match engine.as_mut() {
        Some(engine) => work(engine),
        None => Err(Error::Query("the database is closed".into())),
    }
}

pub struct Database {
    shared: Shared,
}

impl Database {
    fn new(engine: Engine) -> Database {
        Database {
            shared: Arc::new(Mutex::new(Some(engine))),
        }
    }

    fn with<T>(&self, work: impl FnOnce(&mut Engine) -> Result<T>) -> Result<T> {
        with(&self.shared, work)
    }

    /// Reach the pager directly, to damage things behind the engine's back.
    #[cfg(test)]
    pub(super) fn with_pager<T>(&mut self, work: impl FnOnce(&mut crate::pager::Pager) -> T) -> T {
        self.with(|db| Ok(work(&mut db.pager))).unwrap()
    }

    /// Open (or create) a database file, with its log in `<path>-wal`. Fails
    /// with `Error::Locked` if something else has the file open.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        Engine::open(path.as_ref()).map(Database::new)
    }

    /// Open a database on any storage, with a chosen cache size and tracer.
    pub fn open_with(
        storage: Box<dyn Storage>,
        wal_storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Database> {
        Engine::open_with(storage, wal_storage, cache_frames, tracer).map(Database::new)
    }

    /// Use `clock` for commit times from now on, in place of the system's.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        let _ = self.with(|db| {
            db.clock = clock;
            Ok(())
        });
    }

    /// Parse and run exactly one statement.
    pub fn execute(&mut self, sql: &str) -> Result<Output> {
        let statement = parse_one(sql)?;
        self.run(&statement)
    }

    /// Run one parsed statement.
    pub fn run(&mut self, statement: &Statement) -> Result<Output> {
        self.with(|db| db.run(statement))
    }

    /// A reader pinned to the newest commit.
    pub fn reader(&self) -> Result<Reader> {
        let txn = self.with(|db| Ok(db.pin()))?;
        Ok(Reader {
            shared: self.shared.clone(),
            txn,
        })
    }

    /// The newest committed transaction, or 0 if there has been none.
    pub fn latest_txn(&mut self) -> Result<TxnId> {
        self.with(|db| Ok(db.last.txn))
    }

    /// Up to `limit` commits, newest first.
    pub fn commits(&mut self, limit: usize) -> Result<Vec<Commit>> {
        self.with(|db| db.commits.recent(&mut db.pager, limit))
    }

    /// Whether a transaction is waiting for `COMMIT` or `ROLLBACK`.
    pub fn in_transaction(&self) -> bool {
        self.with(|db| Ok(db.in_transaction())).unwrap_or(false)
    }

    /// Every table, sorted by name.
    pub fn tables(&mut self) -> Result<Vec<TableSchema>> {
        self.with(Engine::tables)
    }

    /// Every index, sorted by name.
    pub fn indexes(&mut self) -> Result<Vec<IndexSchema>> {
        self.with(Engine::indexes)
    }

    /// How many committed transactions were recovered from the log on open.
    /// More than zero means the last session ended without a clean close.
    pub fn recovered(&self) -> usize {
        self.with(|db| Ok(db.recovered())).unwrap_or(0)
    }

    /// Move the log's pages into the main file and empty the log. Not during
    /// a transaction, whose frames are in the log and not yet committed.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.with(Engine::checkpoint)
    }

    /// Verify the whole database. See `Engine::check`.
    pub fn check(&mut self) -> Result<CheckReport> {
        self.with(Engine::check)
    }

    /// Roll back any open transaction, checkpoint, and close. After this the
    /// log file is gone and the database is entirely in its main file. Any
    /// readers left get an error from then on.
    pub fn close(self) -> Result<()> {
        let engine = self
            .shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match engine {
            Some(engine) => engine.close(),
            None => Ok(()),
        }
    }
}

impl Drop for Database {
    /// A reader must not keep the file locked after its database has gone.
    /// Dropping without `close` discards uncommitted work, as a crash would.
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

/// A stable view of one moment. Whatever the writer does next, a reader's
/// queries answer as of the transaction it was pinned to, and `VACUUM
/// HISTORY` can't remove that moment until the reader moves on or is dropped.
pub struct Reader {
    shared: Shared,
    txn: TxnId,
}

impl Reader {
    /// The transaction this reader is looking at.
    pub fn txn(&self) -> TxnId {
        self.txn
    }

    /// Parse and run one `SELECT`, or `EXPLAIN SELECT`, at this reader's moment.
    pub fn query(&self, sql: &str) -> Result<Output> {
        self.run(&parse_one(sql)?)
    }

    /// Run one parsed `SELECT`, or `EXPLAIN SELECT`, at this reader's moment.
    pub fn run(&self, statement: &Statement) -> Result<Output> {
        with(&self.shared, |db| db.read(statement, self.txn))
    }

    /// Move up to the newest commit.
    pub fn refresh(&mut self) -> Result<()> {
        self.txn = with(&self.shared, |db| {
            db.unpin(self.txn);
            Ok(db.pin())
        })?;
        Ok(())
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        // If the database has closed, nothing is left to unpin.
        let _ = with(&self.shared, |db| {
            db.unpin(self.txn);
            Ok(())
        });
    }
}

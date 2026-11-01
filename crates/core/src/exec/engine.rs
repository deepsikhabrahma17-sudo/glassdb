//! `Engine`: one open database, shared by its `Database` and `Reader` handles.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::txn::{Txn, failed};
use super::{CheckReport, Output, history, index, no_such_table, select, write};
use crate::btree::BTree;
use crate::catalog::{Catalog, IndexSchema, TableSchema};
use crate::error::{Error, Result};
use crate::mvcc::{self, Commit, CommitLog, TxnId};
use crate::pager::{DEFAULT_CACHE_FRAMES, Pager};
use crate::sql::{CreateTable, Statement, When};
use crate::storage::{FileStorage, Storage};
use crate::time::{Clock, SystemClock};
use crate::trace::{Event, NoopTracer, Tracer};

pub(crate) struct Engine {
    pub(super) pager: Pager,
    pub(super) catalog: Catalog,
    pub(super) commits: CommitLog,
    pub(super) clock: Arc<dyn Clock>,
    /// The log file, removed after a clean close.
    wal_path: Option<PathBuf>,
    pub(super) txn: Txn,
    /// The newest commit, or txn 0 if there has been none.
    pub(super) last: Commit,
    /// Whether the transaction in progress has changed anything yet.
    pub(super) wrote: bool,
    /// How many readers are looking at each transaction. Vacuum can't pass
    /// the oldest, or it would remove rows a reader still needs.
    pub(super) pins: BTreeMap<TxnId, usize>,
}

/// The log's path: `my.db` keeps its log in `my.db-wal`.
fn wal_path(path: &Path) -> PathBuf {
    let mut name = OsString::from(path);
    name.push("-wal");
    PathBuf::from(name)
}

/// Make new files in `path`'s folder durable. Without this a crash could lose
/// the log file's name, and everything committed to it. Best effort, because
/// not every system can sync a folder.
fn sync_folder(path: &Path) {
    let folder = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    if let Ok(folder) = File::open(folder) {
        let _ = folder.sync_all();
    }
}

/// `statement` with its table read at `at`, or why a reader can't run it.
fn pinned(statement: &Statement, at: TxnId) -> Result<Statement> {
    match statement {
        Statement::Select(select) if select.when != When::Now => Err(Error::Query(format!(
            "a reader already has its snapshot, at txn {at}"
        ))),
        Statement::Select(select) => {
            let mut select = select.clone();
            if select.from.is_some() {
                select.when = When::Txn(at);
            }
            Ok(Statement::Select(select))
        }
        Statement::Explain(inner) if matches!(**inner, Statement::Select(_)) => {
            Ok(Statement::Explain(Box::new(pinned(inner, at)?)))
        }
        other => Err(Error::Query(format!(
            "a reader only reads: {other} needs the database"
        ))),
    }
}

impl Engine {
    pub fn open(path: &Path) -> Result<Engine> {
        let log_path = wal_path(path);
        let storage = FileStorage::open(path)?;
        // Before touching the log, which may be in use by whoever holds this.
        storage.lock()?;
        let log = FileStorage::open(&log_path)?;
        sync_folder(path);
        match Engine::open_with(
            Box::new(storage),
            Box::new(log),
            DEFAULT_CACHE_FRAMES,
            Arc::new(NoopTracer),
        ) {
            Ok(mut db) => {
                db.wal_path = Some(log_path);
                Ok(db)
            }
            Err(err) => {
                // Refused (not a database, say): leave no empty log behind.
                if fs::metadata(&log_path).is_ok_and(|m| m.len() == 0) {
                    let _ = fs::remove_file(&log_path);
                }
                Err(err)
            }
        }
    }

    pub fn open_with(
        storage: Box<dyn Storage>,
        wal_storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Engine> {
        let mut pager = Pager::open_with_wal(storage, wal_storage, cache_frames, tracer)?;
        let brand_new = pager.page_count() == 1;
        let catalog = Catalog::open(&mut pager)?;
        let commits = match brand_new {
            true => CommitLog::create(&mut pager)?,
            false => CommitLog::open(),
        };
        pager.flush()?; // a brand-new catalog is committed straight away
        let last = commits.last(&mut pager)?.unwrap_or(Commit {
            txn: 0,
            time: i64::MIN,
        });
        Ok(Engine {
            pager,
            catalog,
            commits,
            clock: Arc::new(SystemClock),
            wal_path: None,
            txn: Txn::None,
            last,
            wrote: false,
            pins: BTreeMap::new(),
        })
    }

    /// Report a statement to the tracer around running it.
    fn traced(
        &mut self,
        statement: &Statement,
        run: impl FnOnce(&mut Engine) -> Result<Output>,
    ) -> Result<Output> {
        self.pager.tracer().event(&Event::Statement {
            sql: statement.to_string(),
        });
        let result = run(self);
        let summary = match &result {
            Ok(output) => output.summary(),
            Err(err) => format!("error: {err}"),
        };
        self.pager.tracer().event(&Event::StatementDone { summary });
        result
    }

    /// Run one parsed statement.
    pub fn run(&mut self, statement: &Statement) -> Result<Output> {
        self.traced(statement, |db| match statement {
            Statement::Begin => db.begin(),
            Statement::Commit => db.commit(),
            Statement::Rollback => db.rollback(),
            _ => db.run_data_statement(statement),
        })
    }

    /// Run a read at transaction `at`, for a reader. It goes straight to the
    /// statement's own work: a read has nothing to commit, and an error here
    /// must not end a transaction the writer has open.
    pub fn read(&mut self, statement: &Statement, at: TxnId) -> Result<Output> {
        self.traced(statement, |db| {
            // A disk error can leave half a statement in the pages, which
            // the stamps can't hide. `ROLLBACK` clears it.
            if db.txn == Txn::Failed {
                return failed();
            }
            db.dispatch(&pinned(statement, at)?)
        })
    }

    /// Hold the newest commit for a reader.
    pub fn pin(&mut self) -> TxnId {
        *self.pins.entry(self.last.txn).or_default() += 1;
        self.last.txn
    }

    pub fn unpin(&mut self, txn: TxnId) {
        if let Some(count) = self.pins.get_mut(&txn) {
            *count -= 1;
            if *count == 0 {
                self.pins.remove(&txn);
            }
        }
    }

    /// Whether `at` is a moment the database can still show exactly.
    pub(super) fn check_past(&mut self, at: TxnId) -> Result<()> {
        if at > self.last.txn {
            return Err(Error::Query(format!(
                "txn {at} hasn't happened (the latest is {})",
                self.last.txn
            )));
        }
        let floor = self.pager.history_floor();
        if at < floor {
            return Err(Error::Query(format!(
                "history before txn {floor} was vacuumed"
            )));
        }
        Ok(())
    }

    /// Every table, sorted by name.
    pub fn tables(&mut self) -> Result<Vec<TableSchema>> {
        self.catalog.list(&mut self.pager)
    }

    /// Every index, sorted by name.
    pub fn indexes(&mut self) -> Result<Vec<IndexSchema>> {
        self.catalog.indexes(&mut self.pager)
    }

    /// How many committed transactions were recovered from the log on open.
    pub fn recovered(&self) -> usize {
        self.pager.recovered()
    }

    /// Move the log's pages into the main file and empty the log. Not during
    /// a transaction, whose frames are in the log and not yet committed.
    pub fn checkpoint(&mut self) -> Result<()> {
        if self.in_transaction() {
            return Err(Error::Query("can't checkpoint inside a transaction".into()));
        }
        self.pager.checkpoint()
    }

    /// Verify the whole database: every B+ tree's invariants, every row's
    /// encoding and version stamps, the commit log, that each index holds
    /// exactly its table's rows, the free list, and that every page is
    /// accounted for.
    pub fn check(&mut self) -> Result<CheckReport> {
        // An open transaction's rows carry the id it will commit under.
        let newest = self.last.txn + TxnId::from(self.wrote);
        let floor = self.pager.history_floor();
        if floor > self.last.txn {
            return Err(Error::Corrupt(format!(
                "the history floor is txn {floor}, past the last commit, txn {}",
                self.last.txn
            )));
        }
        let mut used = 1 + self.catalog.check(&mut self.pager)?; // 1 for the header
        used += self.commits.check(&mut self.pager, self.last.txn)?;
        let tables = self.tables()?;
        let mut rows = 0;
        let mut old_versions = 0;
        let mut indexes = 0;
        for table in &tables {
            let stats = BTree::open(table.root).check(&mut self.pager)?;
            used += stats.internal_nodes + stats.leaves;
            rows += select::scan(&mut self.pager, table)?.len();
            let versions = mvcc::check(&mut self.pager, table, newest, floor)?;
            used += versions.pages;
            old_versions += versions.old_versions;
            let (pages, count) = index::check(self, table)?;
            used += pages;
            indexes += count;
        }
        if let Some(lost) = self.indexes()?.get(indexes) {
            return Err(Error::Corrupt(format!(
                "index {} and others belong to no table",
                lost.name
            )));
        }
        let dropped = self.catalog.dropped(&mut self.pager)?;
        for table in &dropped {
            let stats = BTree::open(table.schema.root).check(&mut self.pager)?;
            used += stats.internal_nodes + stats.leaves;
            let versions = mvcc::check(&mut self.pager, &table.schema, newest, floor)?;
            used += versions.pages;
            old_versions += versions.old_versions;
            let name = &table.schema.name;
            if table.dropped <= table.schema.created || table.dropped > newest {
                return Err(Error::Corrupt(format!(
                    "dropped table {name} has a bad drop transaction, {}",
                    table.dropped
                )));
            }
            if table.dropped <= floor {
                return Err(Error::Corrupt(format!(
                    "dropped table {name} should have been vacuumed"
                )));
            }
        }
        self.pager.check_free_list()?;
        let pages = self.pager.page_count();
        let free_pages = self.pager.free_count();
        if used + free_pages as usize != pages as usize {
            return Err(Error::Corrupt(format!(
                "{used} pages in use and {free_pages} free, but the file has {pages}"
            )));
        }
        Ok(CheckReport {
            tables: tables.len(),
            rows,
            old_versions,
            dropped_tables: dropped.len(),
            pages,
            free_pages,
        })
    }

    /// Roll back any open transaction, checkpoint, and close. After this the
    /// log file is gone and the database is entirely in its main file.
    pub fn close(mut self) -> Result<()> {
        if self.in_transaction() {
            self.pager.rollback()?;
        }
        self.pager.checkpoint()?;
        // The log goes first: the file lock is released when `self` is
        // dropped, and the next opener must not have its new log deleted.
        if let Some(path) = &self.wal_path {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    /// Run a statement's own work, with no commit or transaction handling.
    pub(super) fn dispatch(&mut self, statement: &Statement) -> Result<Output> {
        match statement {
            Statement::Begin | Statement::Commit | Statement::Rollback => {
                unreachable!("`run` handles transaction statements")
            }
            Statement::CreateTable(def) => self.create_table(def),
            Statement::DropTable { name, if_exists } => self.drop_table(name, *if_exists),
            Statement::Insert {
                table,
                columns,
                rows,
            } => write::insert(self, table, columns.as_deref(), rows),
            Statement::Select(select) => select::select(self, select),
            Statement::Update {
                table,
                assignments,
                filter,
            } => write::update(self, table, assignments, filter.as_ref()),
            Statement::Delete { table, filter } => write::delete(self, table, filter.as_ref()),
            Statement::CreateIndex(def) => index::create(self, def),
            Statement::DropIndex { name, if_exists } => index::drop(self, name, *if_exists),
            Statement::Explain(inner) => super::access::explain(self, inner),
            Statement::Vacuum { before } => history::vacuum(self, *before),
        }
    }

    fn create_table(&mut self, def: &CreateTable) -> Result<Output> {
        if def.if_not_exists && self.catalog.get(&mut self.pager, &def.name)?.is_some() {
            return Ok(Output::Message(format!(
                "table {} already exists, skipped",
                def.name
            )));
        }
        let txn = self.next_txn();
        self.catalog.create(&mut self.pager, def, txn)?;
        self.wrote = true;
        Ok(Output::Message(format!("created table {}", def.name)))
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<Output> {
        let txn = self.next_txn();
        match self.catalog.drop(&mut self.pager, name, txn)? {
            Some(_) => {
                self.wrote = true;
                Ok(Output::Message(format!("dropped table {name}")))
            }
            None if if_exists => Ok(Output::Message(format!("no table {name}, skipped"))),
            None => Err(no_such_table(name)),
        }
    }

    /// The schema of `name`, or a "no such table" error.
    pub(super) fn table(&mut self, name: &str) -> Result<TableSchema> {
        self.catalog
            .get(&mut self.pager, name)?
            .ok_or_else(|| no_such_table(name))
    }
}

//! The executor: `Database` runs parsed statements against the catalog and
//! the table B+ trees.
//!
//! Every statement checks everything it can before it writes anything, so a
//! statement that fails (wrong type, duplicate key, ...) changes nothing.
//! Each statement that changes data commits to the write-ahead log when it
//! finishes, so after a crash it is either wholly there or not at all.

mod eval;
mod select;
mod write;

#[cfg(test)]
mod tests;

use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::btree::BTree;
use crate::catalog::{Catalog, TableSchema};
use crate::error::{Error, Result};
use crate::pager::{DEFAULT_CACHE_FRAMES, Pager};
use crate::sql::{CreateTable, Statement, parse_one};
use crate::storage::{FileStorage, Storage};
use crate::trace::{Event, NoopTracer, Tracer};
use crate::types::Value;

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// From CREATE TABLE and DROP TABLE, such as "created table users".
    Message(String),
    Inserted(usize),
    Updated(usize),
    Deleted(usize),
    /// A SELECT's result: column names and rows of values.
    Rows {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
    },
}

impl Output {
    /// A one-line description, for the tracer.
    pub fn summary(&self) -> String {
        match self {
            Output::Rows { rows, .. } => format!("returned {}", plural(rows.len(), "row")),
            other => other.to_string(),
        }
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

impl fmt::Display for Output {
    /// Plain text: messages as-is, and rows as `a | b` lines under a header.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Output::Message(msg) => write!(f, "{msg}"),
            Output::Inserted(n) => write!(f, "inserted {}", plural(*n, "row")),
            Output::Updated(n) => write!(f, "updated {}", plural(*n, "row")),
            Output::Deleted(n) => write!(f, "deleted {}", plural(*n, "row")),
            Output::Rows { columns, rows } => {
                write!(f, "{}", columns.join(" | "))?;
                for row in rows {
                    let cells: Vec<String> = row.iter().map(Value::to_string).collect();
                    write!(f, "\n{}", cells.join(" | "))?;
                }
                Ok(())
            }
        }
    }
}

/// What `Database::check` found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckReport {
    pub tables: usize,
    pub rows: usize,
    /// Pages in the database, including the header page.
    pub pages: u32,
    pub free_pages: u32,
}

pub struct Database {
    pager: Pager,
    catalog: Catalog,
    /// The log file, removed after a clean close.
    wal_path: Option<PathBuf>,
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

impl Database {
    /// Open (or create) a database file, with its log in `<path>-wal`.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        let path = path.as_ref();
        let log_path = wal_path(path);
        let storage = FileStorage::open(path)?;
        let log = FileStorage::open(&log_path)?;
        sync_folder(path);
        match Database::open_with(
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

    /// Open a database on any storage, with a chosen cache size and tracer.
    pub fn open_with(
        storage: Box<dyn Storage>,
        wal_storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Database> {
        let mut pager = Pager::open_with_wal(storage, wal_storage, cache_frames, tracer)?;
        let catalog = Catalog::open(&mut pager)?;
        pager.flush()?; // a brand-new catalog is committed straight away
        Ok(Database {
            pager,
            catalog,
            wal_path: None,
        })
    }

    /// Parse and run exactly one statement.
    pub fn execute(&mut self, sql: &str) -> Result<Output> {
        let statement = parse_one(sql)?;
        self.run(&statement)
    }

    /// Run one parsed statement.
    pub fn run(&mut self, statement: &Statement) -> Result<Output> {
        let tracer = |db: &Database, event: Event| db.pager.tracer().event(&event);
        tracer(
            self,
            Event::Statement {
                sql: statement.to_string(),
            },
        );
        let mut result = self.dispatch(statement);
        if result.is_ok()
            && !matches!(statement, Statement::Select(_))
            && let Err(err) = self.pager.flush()
        {
            result = Err(err);
        }
        let summary = match &result {
            Ok(output) => output.summary(),
            Err(err) => format!("error: {err}"),
        };
        tracer(self, Event::StatementDone { summary });
        result
    }

    /// Every table, sorted by name.
    pub fn tables(&mut self) -> Result<Vec<TableSchema>> {
        self.catalog.list(&mut self.pager)
    }

    /// How many committed transactions were recovered from the log on open.
    /// More than zero means the last session ended without a clean close.
    pub fn recovered(&self) -> usize {
        self.pager.recovered()
    }

    /// Move the log's pages into the main file and empty the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.pager.checkpoint()
    }

    /// Verify the whole database: every B+ tree's invariants, every row's
    /// encoding, the free list, and that every page is accounted for.
    pub fn check(&mut self) -> Result<CheckReport> {
        let mut used = 1 + self.catalog.check(&mut self.pager)?; // 1 for the header
        let tables = self.tables()?;
        let mut rows = 0;
        for table in &tables {
            let stats = BTree::open(table.root).check(&mut self.pager)?;
            used += stats.internal_nodes + stats.leaves;
            rows += select::scan(&mut self.pager, table)?.len();
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
            pages,
            free_pages,
        })
    }

    /// Commit everything, checkpoint, and close. After this the log file is
    /// gone and the database is entirely in its main file.
    pub fn close(self) -> Result<()> {
        self.pager.close()?;
        if let Some(path) = self.wal_path {
            let _ = fs::remove_file(path);
        }
        Ok(())
    }

    fn dispatch(&mut self, statement: &Statement) -> Result<Output> {
        match statement {
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
        }
    }

    fn create_table(&mut self, def: &CreateTable) -> Result<Output> {
        if def.if_not_exists && self.catalog.get(&mut self.pager, &def.name)?.is_some() {
            return Ok(Output::Message(format!(
                "table {} already exists, skipped",
                def.name
            )));
        }
        self.catalog.create(&mut self.pager, def)?;
        Ok(Output::Message(format!("created table {}", def.name)))
    }

    fn drop_table(&mut self, name: &str, if_exists: bool) -> Result<Output> {
        match self.catalog.drop(&mut self.pager, name)? {
            Some(_) => Ok(Output::Message(format!("dropped table {name}"))),
            None if if_exists => Ok(Output::Message(format!("no table {name}, skipped"))),
            None => Err(no_such_table(name)),
        }
    }

    /// The schema of `name`, or a "no such table" error.
    fn table(&mut self, name: &str) -> Result<TableSchema> {
        self.catalog
            .get(&mut self.pager, name)?
            .ok_or_else(|| no_such_table(name))
    }
}

fn no_such_table(name: &str) -> Error {
    Error::Query(format!("no such table: {name}"))
}

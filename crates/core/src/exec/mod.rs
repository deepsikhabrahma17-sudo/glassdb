//! The executor: `Database` runs parsed statements against the catalog and
//! the table B+ trees.
//!
//! Every statement checks everything it can before it writes anything, so a
//! statement that fails (wrong type, duplicate key, ...) changes nothing.
//! Each statement that changes data is flushed to disk when it finishes.

mod eval;
mod select;
mod write;

#[cfg(test)]
mod tests;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

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

pub struct Database {
    pager: Pager,
    catalog: Catalog,
}

impl Database {
    /// Open (or create) a database file.
    pub fn open(path: impl AsRef<Path>) -> Result<Database> {
        let storage = FileStorage::open(path)?;
        Database::open_with(
            Box::new(storage),
            DEFAULT_CACHE_FRAMES,
            Arc::new(NoopTracer),
        )
    }

    /// Open a database on any storage, with a chosen cache size and tracer.
    pub fn open_with(
        storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Database> {
        let mut pager = Pager::open(storage, cache_frames, tracer)?;
        let catalog = Catalog::open(&mut pager)?;
        pager.flush()?; // a brand-new catalog goes to disk straight away
        Ok(Database { pager, catalog })
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

    /// Flush everything and close the file.
    pub fn close(self) -> Result<()> {
        self.pager.close()
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

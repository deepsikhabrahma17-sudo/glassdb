//! The executor: `Engine` runs parsed statements against the catalog and the
//! table B+ trees, and `Database` and `Reader` are the handles that share it.
//!
//! Every statement checks everything it can before it writes anything, so a
//! statement that fails (wrong type, duplicate key, ...) changes nothing.
//! Each statement that changes data commits to the write-ahead log when it
//! finishes, or at `COMMIT` inside a transaction (see `txn`), so after a crash
//! it is either wholly there or not at all.
//!
//! A database file is locked for as long as it is open, because opening one
//! checkpoints and empties its log. Within a process, `&mut Database` means
//! one writer at a time, and readers look at a moment that writer can't touch.

mod access;
mod engine;
pub(crate) mod eval;
mod handle;
mod history;
mod index;
mod select;
mod txn;
mod write;

#[cfg(test)]
mod index_tests;
#[cfg(test)]
mod reader_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod time_tests;
#[cfg(test)]
mod txn_tests;

use std::fmt;

use crate::error::Error;
use crate::types::Value;
use engine::Engine;
pub use handle::{Database, Reader};

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    /// From CREATE and DROP, and transaction statements: "created table users".
    Message(String),
    Inserted(usize),
    Updated(usize),
    Deleted(usize),
    /// A SELECT's result, or EXPLAIN's steps: column names and rows of values.
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
    /// Replaced row versions kept for `AS OF`.
    pub old_versions: usize,
    /// Dropped tables kept for `AS OF`.
    pub dropped_tables: usize,
    /// Pages in the database, including the header page.
    pub pages: u32,
    pub free_pages: u32,
}

fn no_such_table(name: &str) -> Error {
    Error::Query(format!("no such table: {name}"))
}

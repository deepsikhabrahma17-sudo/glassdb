//! GlassDB engine library.
//!
//! Layers, bottom-up (see docs/DESIGN.md for formats and decisions):
//! `storage` (raw bytes), `wal` (the write-ahead log that makes commits safe),
//! `pager` (4 KB pages, buffer cache), `btree`
//! (ordered keys in pages), `types` (SQL values, row and index encoding),
//! `mvcc` (row versions and the commit log), `catalog` (the list of tables and
//! indexes), `sql` (tokenizer and parser), `planner` (how to find a statement's
//! rows) and `exec` (the `Database` that runs statements). `trace` is the event
//! stream every layer reports to, and `time` is the clock commits are stamped
//! with.

pub mod btree;
mod bytes;
pub mod catalog;
pub mod error;
pub mod exec;
pub mod mvcc;
pub mod pager;
pub mod planner;
pub mod sql;
pub mod storage;
pub mod time;
pub mod trace;
pub mod types;
pub mod wal;

pub use btree::{BTree, TreeStats};
pub use catalog::{Catalog, IndexSchema, TableSchema};
pub use error::{Error, Result};
pub use exec::{CheckReport, Database, Output, Reader};
pub use mvcc::{Commit, TxnId};
pub use pager::{PAGE_SIZE, Page, PageId, Pager};
pub use storage::{FileStorage, MemStorage, Storage};
pub use time::{Clock, ManualClock, SystemClock};
pub use trace::{Event, NoopTracer, RecordingTracer, Tracer};
pub use types::{DataType, Value};

/// Engine version, shown in the CLI banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

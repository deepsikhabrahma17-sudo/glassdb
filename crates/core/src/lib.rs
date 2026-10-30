//! GlassDB engine library.
//!
//! Layers, bottom-up (see docs/DESIGN.md for formats and decisions):
//! `storage` (raw bytes), `wal` (the write-ahead log that makes commits safe),
//! `pager` (4 KB pages, buffer cache), `btree`
//! (ordered keys in pages), `types` (SQL values and row encoding), `catalog`
//! (the list of tables), `sql` (tokenizer and parser) and `exec` (the
//! `Database` that runs statements). `trace` is the event stream every layer
//! reports to.

pub mod btree;
mod bytes;
pub mod catalog;
pub mod error;
pub mod exec;
pub mod pager;
pub mod sql;
pub mod storage;
pub mod trace;
pub mod types;
pub mod wal;

pub use btree::{BTree, TreeStats};
pub use catalog::{Catalog, TableSchema};
pub use error::{Error, Result};
pub use exec::{CheckReport, Database, Output};
pub use pager::{PAGE_SIZE, Page, PageId, Pager};
pub use storage::{FileStorage, MemStorage, Storage};
pub use trace::{Event, NoopTracer, RecordingTracer, Tracer};
pub use types::{DataType, Value};

/// Engine version, shown in the CLI banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

//! GlassDB engine library.
//!
//! Layers are added bottom-up; see docs/DESIGN.md for the on-disk format.
//! So far: `storage` (raw bytes), `pager` (4 KB pages and the buffer cache)
//! and `trace` (the event stream every layer reports to).

pub mod error;
pub mod pager;
pub mod storage;
pub mod trace;

pub use error::{Error, Result};
pub use pager::{PAGE_SIZE, Page, PageId, Pager};
pub use storage::{FileStorage, MemStorage, Storage};
pub use trace::{Event, NoopTracer, RecordingTracer, Tracer};

/// Engine version, shown in the CLI banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

//! GlassDB engine library.
//!
//! The storage layers (pager, B+ tree, WAL, MVCC) are added phase by phase;
//! see docs/DESIGN.md for the on-disk format as it grows.

/// Engine version, shown in the CLI banner.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

//! Errors returned by the engine.

use std::fmt;
use std::io;

use crate::btree::node::{MAX_ENTRY, MAX_KEY};
use crate::pager::PageId;

#[derive(Debug)]
pub enum Error {
    /// The operating system reported a read, write or sync failure.
    Io(io::Error),
    /// The file does not start with the GlassDB magic bytes.
    BadMagic,
    /// The file was written by a format version this build cannot read.
    UnsupportedVersion(u16),
    /// The file uses a page size other than the one this build is compiled for.
    BadPageSize(u16),
    /// A page id outside 1..page_count (page 0 is the header, owned by the pager).
    PageOutOfRange(PageId),
    /// The file's contents contradict themselves.
    Corrupt(String),
    /// Every page id is in use.
    DatabaseFull,
    /// A B+ tree entry over the size limits (`MAX_KEY`, `MAX_ENTRY`).
    EntryTooLarge { key_len: usize, value_len: usize },
}

pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "I/O error: {err}"),
            Error::BadMagic => write!(f, "not a GlassDB database file (bad magic bytes)"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported file format version {v}"),
            Error::BadPageSize(size) => write!(f, "unsupported page size {size}"),
            Error::PageOutOfRange(id) => write!(f, "page {id} is out of range"),
            Error::Corrupt(msg) => write!(f, "database file is corrupt: {msg}"),
            Error::DatabaseFull => write!(f, "database is full: no page ids left"),
            Error::EntryTooLarge { key_len, value_len } => write!(
                f,
                "entry too large: key {key_len} bytes, value {value_len} bytes \
                 (limits: key {MAX_KEY}, key + value {MAX_ENTRY})"
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

//! The write-ahead log: changed pages go here first, then a commit record.
//!
//! The rule that makes crashes safe: a transaction counts only once its commit
//! record is on disk. Anything after the last valid commit record is thrown
//! away on recovery, so a transaction is either wholly there or not at all.
//!
//! File layout (all integers little-endian):
//!
//! | bytes  | header field                  |
//! |--------|-------------------------------|
//! | 0..8   | magic `GLASSWAL`              |
//! | 8..12  | format version                |
//! | 12..16 | zero                          |
//!
//! Then records, one after another:
//!
//! | bytes  | record field                                            |
//! |--------|---------------------------------------------------------|
//! | 0      | kind: 1 = page frame, 2 = commit                        |
//! | 1..4   | zero                                                    |
//! | 4..8   | page frame: page id; commit: frames in this transaction |
//! | 8..12  | checksum                                                |
//! | 12..   | page frame only: the 4096-byte page                     |
//!
//! Checksums are CRC32 and *chained*: each covers the previous record's
//! checksum as well as its own bytes. So a torn, damaged, or out-of-place
//! record fails the check, and recovery stops at the first one that does.

#[cfg(test)]
mod tests;

use std::collections::HashMap;

use crc32fast::Hasher;

use crate::bytes::{get_u32, put_u32};
use crate::error::{Error, Result};
use crate::pager::{PAGE_SIZE, Page, PageId};
use crate::storage::Storage;

const MAGIC: [u8; 8] = *b"GLASSWAL";
const VERSION: u32 = 1;
const HEADER_SIZE: u64 = 16;
const RECORD_HEADER: usize = 12;
const FRAME_SIZE: usize = RECORD_HEADER + PAGE_SIZE;
const KIND_PAGE: u8 = 1;
const KIND_COMMIT: u8 = 2;

/// What a scan of the log found when it was opened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Recovery {
    /// Transactions with a valid commit record.
    pub transactions: usize,
    /// Page frames in those transactions.
    pub frames: usize,
    /// Valid frames after the last commit record, thrown away.
    pub discarded: usize,
}

pub struct Wal {
    storage: Box<dyn Storage>,
    /// Bytes in the log file, which can exceed `end` if a torn tail is left.
    size: u64,
    /// Where the next record goes.
    end: u64,
    /// Checksum of the last record, which the next one chains from.
    crc: u32,
    /// Where each page's newest committed frame's bytes are.
    committed: HashMap<PageId, u64>,
    /// The same for the running transaction. Checked first by `read`.
    pending: HashMap<PageId, u64>,
    /// Page frames in the log, committed or not.
    frames: u32,
    /// Page frames in the running transaction.
    txn_frames: u32,
}

/// The first 8 bytes of a record.
fn record_head(kind: u8, value: u32) -> [u8; 8] {
    let mut head = [0u8; 8];
    head[0] = kind;
    put_u32(&mut head, 4, value);
    head
}

/// The checksum of a record, chained from `previous`.
fn checksum(previous: u32, head: &[u8], payload: &[u8]) -> u32 {
    let mut hasher = Hasher::new_with_initial(previous);
    hasher.update(head);
    hasher.update(payload);
    hasher.finalize()
}

impl Wal {
    /// Open a log and scan it for committed transactions. Reading from the
    /// result then gives the committed pages. A log with a damaged header
    /// can't hold anything that was committed, so it counts as empty.
    pub fn open(mut storage: Box<dyn Storage>) -> Result<(Wal, Recovery)> {
        let size = storage.size()?;
        let mut wal = Wal {
            storage,
            size,
            end: 0,
            crc: 0,
            committed: HashMap::new(),
            pending: HashMap::new(),
            frames: 0,
            txn_frames: 0,
        };
        let mut recovery = Recovery::default();
        if size < HEADER_SIZE {
            return Ok((wal, recovery));
        }
        let mut header = [0u8; HEADER_SIZE as usize];
        wal.storage.read_at(0, &mut header)?;
        if header[..8] != MAGIC {
            return Ok((wal, recovery));
        }
        let version = get_u32(&header, 8);
        if version == 0 {
            // The header was torn after the magic: nothing was ever committed.
            return Ok((wal, recovery));
        }
        if version != VERSION {
            return Err(Error::UnsupportedVersion(version as u16));
        }

        // Walk the records until one is incomplete or fails its checksum.
        let mut pos = HEADER_SIZE;
        let mut crc = 0;
        let mut txn: Vec<(PageId, u64)> = Vec::new();
        while pos + RECORD_HEADER as u64 <= size {
            let mut head = [0u8; RECORD_HEADER];
            wal.storage.read_at(pos, &mut head)?;
            let (value, stored) = (get_u32(&head, 4), get_u32(&head, 8));
            match head[0] {
                KIND_PAGE if pos + FRAME_SIZE as u64 <= size => {
                    let mut page = Box::new([0u8; PAGE_SIZE]);
                    wal.storage
                        .read_at(pos + RECORD_HEADER as u64, &mut page[..])?;
                    let computed = checksum(crc, &head[..8], &page[..]);
                    if computed != stored {
                        break;
                    }
                    crc = computed;
                    txn.push((value, pos + RECORD_HEADER as u64));
                    pos += FRAME_SIZE as u64;
                }
                KIND_COMMIT => {
                    let computed = checksum(crc, &head[..8], &[]);
                    if computed != stored || value as usize != txn.len() {
                        break;
                    }
                    crc = computed;
                    pos += RECORD_HEADER as u64;
                    // Later frames of a page overwrite earlier ones here.
                    recovery.transactions += 1;
                    recovery.frames += txn.len();
                    wal.frames += value;
                    wal.committed.extend(txn.drain(..));
                    wal.end = pos;
                    wal.crc = crc;
                }
                _ => break,
            }
        }
        recovery.discarded = txn.len();
        Ok((wal, recovery))
    }

    /// Add a page to the running transaction.
    pub fn append_page(&mut self, id: PageId, page: &Page) -> Result<()> {
        let head = record_head(KIND_PAGE, id);
        let crc = checksum(self.crc, &head, page);
        let mut record = Vec::with_capacity(FRAME_SIZE);
        record.extend(head);
        record.extend(crc.to_le_bytes());
        record.extend(page);
        let start = self.write_record(&record)?;
        self.crc = crc;
        self.pending.insert(id, start + RECORD_HEADER as u64);
        self.frames += 1;
        self.txn_frames += 1;
        Ok(())
    }

    /// End the running transaction: write the commit record and force the
    /// log to disk. When this returns, the transaction survives any crash.
    /// Returns how many frames it held (0 means there was nothing to commit).
    pub fn commit(&mut self) -> Result<u32> {
        let frames = self.txn_frames;
        if frames == 0 {
            return Ok(0);
        }
        let head = record_head(KIND_COMMIT, frames);
        let crc = checksum(self.crc, &head, &[]);
        let mut record = Vec::with_capacity(RECORD_HEADER);
        record.extend(head);
        record.extend(crc.to_le_bytes());
        self.write_record(&record)?;
        self.crc = crc;
        self.storage.sync()?;
        self.committed.extend(self.pending.drain());
        self.txn_frames = 0;
        Ok(frames)
    }

    /// Copy page `id` into `page`, newest version first. Returns false if
    /// the log doesn't hold that page.
    pub fn read(&mut self, id: PageId, page: &mut Page) -> Result<bool> {
        match self.pending.get(&id).or_else(|| self.committed.get(&id)) {
            Some(&at) => {
                self.storage.read_at(at, page)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Every committed page, in ascending order.
    pub fn pages(&self) -> Vec<PageId> {
        let mut pages: Vec<PageId> = self.committed.keys().copied().collect();
        pages.sort_unstable();
        pages
    }

    /// Page frames in the log, committed or not.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// Empty the log, for after its pages are safely in the main file.
    pub fn reset(&mut self) -> Result<()> {
        if self.size > 0 {
            self.storage.truncate(0)?;
            self.storage.sync()?;
        }
        self.size = 0;
        self.end = 0;
        self.crc = 0;
        self.committed.clear();
        self.pending.clear();
        self.frames = 0;
        self.txn_frames = 0;
        Ok(())
    }

    /// Write `record` at the end, adding the file header first if the log is
    /// empty. Returns where the record starts.
    fn write_record(&mut self, record: &[u8]) -> Result<u64> {
        if self.end == 0 {
            let mut header = [0u8; HEADER_SIZE as usize];
            header[..8].copy_from_slice(&MAGIC);
            put_u32(&mut header, 8, VERSION);
            self.storage.write_at(0, &header)?;
            self.end = HEADER_SIZE;
        }
        let start = self.end;
        self.storage.write_at(start, record)?;
        self.end += record.len() as u64;
        self.size = self.size.max(self.end);
        Ok(start)
    }
}

//! The pager: fixed-size pages on top of a `Storage`, with a buffer cache.
//!
//! Page 0 is the file header (see `header`) and belongs to the pager; callers
//! use pages 1..page_count. Pages are copied in and out: `read_page` returns
//! an owned copy and `write_page` copies it back, so callers never hold a
//! borrow into the cache.
//!
//! With a write-ahead log (`open_with_wal`), the main file changes only during
//! a checkpoint. A read looks in the cache, then the log, then the main file.
//! A changed page that must leave the cache goes to the log, and `flush`
//! commits, or `rollback` throws the changes away. Without a log (`open`),
//! pages go straight to the main file, which is simpler and faster but not
//! crash safe and can't roll back; the lower-level tests use it.

pub mod cache;
pub mod header;
mod open;

#[cfg(test)]
mod rollback_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod wal_tests;

use std::sync::Arc;

use crate::bytes::{get_u32, put_u32};
use crate::error::{Error, Result};
use crate::storage::Storage;
use crate::trace::{Event, Tracer};
use crate::wal::Wal;
use cache::{Cache, Frame};
use header::Header;

pub const PAGE_SIZE: usize = 4096;
pub type PageId = u32;
pub type Page = [u8; PAGE_SIZE];

/// Cache frames used by `Pager::open_file`: 256 × 4 KB = 1 MB.
pub const DEFAULT_CACHE_FRAMES: usize = 256;

/// A commit that leaves the log this many frames long (about 4 MB) also
/// checkpoints it, so the log can't grow without limit.
pub const CHECKPOINT_FRAMES: u32 = 1000;

pub struct Pager {
    storage: Box<dyn Storage>,
    /// In-memory copy of page 0; written back by `flush` when `header_dirty`.
    header: Header,
    header_dirty: bool,
    /// The header as of the last commit, which a rollback restores.
    committed_header: Header,
    cache: Cache,
    tracer: Arc<dyn Tracer>,
    wal: Option<Wal>,
    /// Transactions found in the log when this pager opened.
    recovered: usize,
}

impl Pager {
    /// Number of pages, including the header page.
    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }

    /// Number of pages on the free list.
    pub fn free_count(&self) -> u32 {
        self.header.free_count
    }

    /// How many committed transactions the log held when this pager opened.
    /// More than zero means the last session ended without a clean close.
    pub fn recovered(&self) -> usize {
        self.recovered
    }

    /// Follow the free list and check it matches the header's count.
    pub fn check_free_list(&mut self) -> Result<()> {
        let mut page = self.header.free_head;
        for _ in 0..self.header.free_count {
            if page == 0 || page >= self.header.page_count {
                return Err(Error::Corrupt(format!("free list reaches page {page}")));
            }
            page = get_u32(&self.read_page(page)?[..], 0);
        }
        if page != 0 {
            return Err(Error::Corrupt(format!(
                "free list is longer than its count of {}",
                self.header.free_count
            )));
        }
        Ok(())
    }

    /// The tracer every layer above the pager reports to.
    pub fn tracer(&self) -> &dyn Tracer {
        &*self.tracer
    }

    /// Return a copy of page `id`.
    pub fn read_page(&mut self, id: PageId) -> Result<Box<Page>> {
        let frame = self.load(id, true)?;
        Ok(frame.data.clone())
    }

    /// Replace page `id`. The change stays in the cache until evicted or flushed.
    pub fn write_page(&mut self, id: PageId, data: &Page) -> Result<()> {
        // The whole page is replaced, so a miss needn't read the old bytes.
        let frame = self.load(id, false)?;
        frame.data.copy_from_slice(data);
        frame.dirty = true;
        Ok(())
    }

    /// Get a zeroed page: the most recently freed one, or a new one at the end.
    pub fn allocate(&mut self) -> Result<PageId> {
        let id = if self.header.free_head != 0 {
            // Pop the head of the free list; its first 4 bytes name the next free page.
            let id = self.header.free_head;
            let next = get_u32(&self.read_page(id)?[..], 0);
            if next >= self.header.page_count {
                return Err(Error::Corrupt(format!(
                    "free page {id} points to page {next}, past the end"
                )));
            }
            let remaining =
                self.header.free_count.checked_sub(1).ok_or_else(|| {
                    Error::Corrupt("free list is longer than the free count".into())
                })?;
            self.header.free_head = next;
            self.header.free_count = remaining;
            id
        } else {
            let id = self.header.page_count;
            if id == PageId::MAX {
                return Err(Error::DatabaseFull);
            }
            self.header.page_count += 1;
            id
        };
        self.header_dirty = true;
        self.write_page(id, &[0; PAGE_SIZE])?;
        Ok(id)
    }

    /// Put page `id` on the free list for reuse. Freeing a page twice
    /// corrupts the list; callers must not do it.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        self.check_range(id)?;
        // A free page holds only the id of the next free page; the rest is zeroed.
        let mut page = [0u8; PAGE_SIZE];
        put_u32(&mut page, 0, self.header.free_head);
        self.write_page(id, &page)?;
        self.header.free_head = id;
        self.header.free_count += 1;
        self.header_dirty = true;
        Ok(())
    }

    /// Make every change so far durable. With a log, this commits a
    /// transaction; without one, it writes straight to the main file.
    pub fn flush(&mut self) -> Result<()> {
        let Some(wal) = &mut self.wal else {
            return self.write_to_main();
        };
        for id in self.cache.dirty_pages() {
            let frame = self.cache.peek_mut(id).expect("dirty page is cached");
            wal.append_page(id, &frame.data)?;
            frame.dirty = false;
            self.tracer.event(&Event::WalAppend { page: id });
        }
        if self.header_dirty {
            wal.append_page(0, &self.header.encode())?;
            self.header_dirty = false;
            self.tracer.event(&Event::WalAppend { page: 0 });
        }
        let frames = wal.commit()?;
        self.committed_header = self.header;
        if frames > 0 {
            self.tracer.event(&Event::WalCommit { frames });
        }
        if wal.frames() >= CHECKPOINT_FRAMES {
            // The commit is already safe, so a failure here mustn't report it
            // as failed. The log stays whole and the next commit tries again.
            let _ = copy_log_to_main(wal, self.storage.as_mut(), self.tracer.as_ref());
        }
        Ok(())
    }

    /// Throw away every change since the last commit. Returns how many frames
    /// had already spilled to the log. Needs a log: without one, spilled pages
    /// have overwritten the main file and can't be taken back.
    pub fn rollback(&mut self) -> Result<u32> {
        let Some(wal) = &mut self.wal else {
            return Err(Error::Query("rollback needs a write-ahead log".into()));
        };
        // Every frame goes, not just the dirty ones: a page that spilled and
        // was read back is clean in the cache but still uncommitted.
        self.cache.clear();
        self.header = self.committed_header;
        self.header_dirty = false;
        let frames = wal.rollback()?;
        self.tracer.event(&Event::TxnRollback { frames });
        Ok(frames)
    }

    /// Commit, then move the log's pages into the main file and empty the log.
    pub fn checkpoint(&mut self) -> Result<()> {
        self.flush()?;
        if let Some(wal) = &mut self.wal {
            copy_log_to_main(wal, self.storage.as_mut(), self.tracer.as_ref())?;
        }
        Ok(())
    }

    /// Checkpoint and close. Dropping a pager without `close` discards
    /// everything since the last `flush`, just as a crash would.
    pub fn close(mut self) -> Result<()> {
        self.checkpoint()
    }

    /// Without a log: write every changed page, then the header, then sync.
    fn write_to_main(&mut self) -> Result<()> {
        // Pages go before the header, so the header never counts pages that
        // are not in the file yet. Ascending order keeps the writes sequential.
        for id in self.cache.dirty_pages() {
            let frame = self.cache.peek_mut(id).expect("dirty page is cached");
            self.storage.write_at(offset(id), &frame.data[..])?;
            frame.dirty = false;
            self.tracer.event(&Event::PageWrite { page: id });
        }
        if self.header_dirty {
            self.storage.write_at(0, &self.header.encode()[..])?;
            self.header_dirty = false;
            self.tracer.event(&Event::PageWrite { page: 0 });
        }
        self.storage.sync()
    }

    fn check_range(&self, id: PageId) -> Result<()> {
        if id == 0 || id >= self.header.page_count {
            return Err(Error::PageOutOfRange(id));
        }
        Ok(())
    }

    /// Bring page `id` into the cache and return its frame, marked most
    /// recently used. On a miss the old bytes are read from storage only if
    /// `read_from_disk`; otherwise the caller must overwrite the whole page.
    fn load(&mut self, id: PageId, read_from_disk: bool) -> Result<&mut Frame> {
        self.check_range(id)?;
        if self.cache.contains(id) {
            self.tracer.event(&Event::CacheHit { page: id });
            return Ok(self.cache.get(id).expect("checked above"));
        }
        self.tracer.event(&Event::CacheMiss { page: id });

        // Make room first, so events read: miss → evict (→ write back) → read.
        let mut data = match self.cache.evict_if_full() {
            Some(victim) => {
                self.tracer.event(&Event::CacheEvict {
                    page: victim.page,
                    dirty: victim.dirty,
                });
                if victim.dirty {
                    // With a log this frame has no commit record yet, so a
                    // crash drops it and the main file never sees it.
                    let saved = match &mut self.wal {
                        Some(wal) => wal.append_page(victim.page, &victim.data),
                        None => self.storage.write_at(offset(victim.page), &victim.data[..]),
                    };
                    if let Err(err) = saved {
                        // Keep the unsaved page rather than lose it.
                        self.cache.insert(victim);
                        return Err(err);
                    }
                    self.tracer.event(&match self.wal {
                        Some(_) => Event::WalAppend { page: victim.page },
                        None => Event::PageWrite { page: victim.page },
                    });
                }
                // Reuse the victim's buffer for the incoming page.
                victim.data
            }
            None => Box::new([0; PAGE_SIZE]),
        };
        if read_from_disk {
            let in_log = match &mut self.wal {
                Some(wal) => wal.read(id, &mut data)?,
                None => false,
            };
            if in_log {
                self.tracer.event(&Event::WalRead { page: id });
            } else {
                self.storage.read_at(offset(id), &mut data[..])?;
                self.tracer.event(&Event::PageRead { page: id });
            }
        }
        self.cache.insert(Frame {
            page: id,
            data,
            dirty: false,
        });
        Ok(self.cache.get(id).expect("just inserted"))
    }
}

/// Byte offset of page `id` in the file.
fn offset(id: PageId) -> u64 {
    u64::from(id) * PAGE_SIZE as u64
}

/// Copy every committed page from the log into the main file, then empty the
/// log. The order is what makes a crash here safe: the log is only emptied
/// after the main file is synced, so until then the log can redo everything.
fn copy_log_to_main(wal: &mut Wal, storage: &mut dyn Storage, tracer: &dyn Tracer) -> Result<()> {
    let pages = wal.pages();
    let mut page = Box::new([0u8; PAGE_SIZE]);
    for &id in &pages {
        wal.read(id, &mut page)?;
        storage.write_at(offset(id), &page[..])?;
        tracer.event(&Event::PageWrite { page: id });
    }
    if !pages.is_empty() {
        storage.sync()?;
        tracer.event(&Event::Checkpoint {
            pages: pages.len() as u32,
        });
    }
    wal.reset()
}

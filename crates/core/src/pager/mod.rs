//! The pager: fixed-size pages on top of a `Storage`, with a buffer cache.
//!
//! Page 0 is the file header (see `header`) and belongs to the pager; callers
//! use pages 1..page_count. Pages are copied in and out: `read_page` returns
//! an owned copy and `write_page` copies it back, so callers never hold a
//! borrow into the cache.

pub mod cache;
pub mod header;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;

use crate::bytes::{get_u32, put_u32};
use crate::error::{Error, Result};
use crate::storage::{FileStorage, Storage};
use crate::trace::{Event, NoopTracer, Tracer};
use cache::{Cache, Frame};
use header::{Header, MAGIC};

pub const PAGE_SIZE: usize = 4096;
pub type PageId = u32;
pub type Page = [u8; PAGE_SIZE];

/// Cache frames used by `Pager::open_file`: 256 × 4 KB = 1 MB.
pub const DEFAULT_CACHE_FRAMES: usize = 256;

pub struct Pager {
    storage: Box<dyn Storage>,
    /// In-memory copy of page 0; written back by `flush` when `header_dirty`.
    header: Header,
    header_dirty: bool,
    cache: Cache,
    tracer: Arc<dyn Tracer>,
}

impl Pager {
    /// Open (or create) a database file with the default cache and no tracing.
    pub fn open_file(path: impl AsRef<Path>) -> Result<Pager> {
        let storage = FileStorage::open(path)?;
        Pager::open(
            Box::new(storage),
            DEFAULT_CACHE_FRAMES,
            Arc::new(NoopTracer),
        )
    }

    /// Open a database on `storage`. Empty storage becomes a new database.
    pub fn open(
        mut storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Pager> {
        let size = storage.size()?;
        let header = if size == 0 {
            // Brand-new file: write the header now so the file is valid at once.
            let header = Header::new_database();
            storage.write_at(0, &header.encode()[..])?;
            storage.sync()?;
            tracer.event(&Event::PageWrite { page: 0 });
            header
        } else {
            if size < PAGE_SIZE as u64 {
                // Too short for a header. If it doesn't even start with the
                // magic, it was never a GlassDB file (say, a text file).
                let mut start = vec![0u8; size.min(MAGIC.len() as u64) as usize];
                storage.read_at(0, &mut start)?;
                if start != MAGIC {
                    return Err(Error::BadMagic);
                }
                return Err(Error::Corrupt(format!(
                    "file is {size} bytes, smaller than one page"
                )));
            }
            let mut page = Box::new([0u8; PAGE_SIZE]);
            storage.read_at(0, &mut page[..])?;
            tracer.event(&Event::PageRead { page: 0 });
            let header = Header::decode(&page)?;
            // A longer file is fine (a flush may have stopped before the
            // header was updated); a shorter one has lost pages.
            let needed = offset(header.page_count);
            if size < needed {
                return Err(Error::Corrupt(format!(
                    "header says {} pages ({needed} bytes) but the file is {size} bytes",
                    header.page_count
                )));
            }
            header
        };
        Ok(Pager {
            storage,
            header,
            header_dirty: false,
            cache: Cache::new(cache_frames),
            tracer,
        })
    }

    /// Number of pages, including the header page.
    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }

    /// Number of pages on the free list.
    pub fn free_count(&self) -> u32 {
        self.header.free_count
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

    /// Write every changed page, then the header, then sync to disk.
    pub fn flush(&mut self) -> Result<()> {
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

    /// Flush and close. Dropping a pager without `close` discards unflushed changes.
    pub fn close(mut self) -> Result<()> {
        self.flush()
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
                    if let Err(err) = self.storage.write_at(offset(victim.page), &victim.data[..]) {
                        // Keep the unsaved page rather than lose it.
                        self.cache.insert(victim);
                        return Err(err);
                    }
                    self.tracer.event(&Event::PageWrite { page: victim.page });
                }
                // Reuse the victim's buffer for the incoming page.
                victim.data
            }
            None => Box::new([0; PAGE_SIZE]),
        };
        if read_from_disk {
            self.storage.read_at(offset(id), &mut data[..])?;
            self.tracer.event(&Event::PageRead { page: id });
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

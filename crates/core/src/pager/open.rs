//! Opening a pager: recover the log if there is one, then read the header.

use std::path::Path;
use std::sync::Arc;

use super::header::{Header, MAGIC};
use super::{DEFAULT_CACHE_FRAMES, PAGE_SIZE, Pager, copy_log_to_main, offset};
use crate::error::{Error, Result};
use crate::pager::cache::Cache;
use crate::storage::{FileStorage, Storage};
use crate::trace::{Event, NoopTracer, Tracer};
use crate::wal::{Recovery, Wal};

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

    /// Open a database on `storage`, with no log. Empty storage becomes a new
    /// database. Not crash safe: see the module comment.
    pub fn open(
        storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Pager> {
        Pager::start(storage, None, 0, cache_frames, tracer)
    }

    /// Open a crash-safe database. If `wal_storage` holds a log left by a
    /// crash, its committed transactions are moved into the main file first,
    /// and anything after the last commit record is thrown away.
    pub fn open_with_wal(
        mut storage: Box<dyn Storage>,
        wal_storage: Box<dyn Storage>,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Pager> {
        let (mut wal, recovery) = Wal::open(wal_storage)?;
        if recovery != Recovery::default() {
            tracer.event(&Event::WalRecovered {
                transactions: recovery.transactions,
                frames: recovery.frames,
                discarded: recovery.discarded,
            });
        }
        copy_log_to_main(&mut wal, storage.as_mut(), tracer.as_ref())?;
        Pager::start(
            storage,
            Some(wal),
            recovery.transactions,
            cache_frames,
            tracer,
        )
    }

    fn start(
        mut storage: Box<dyn Storage>,
        wal: Option<Wal>,
        recovered: usize,
        cache_frames: usize,
        tracer: Arc<dyn Tracer>,
    ) -> Result<Pager> {
        let size = storage.size()?;
        let mut header_dirty = false;
        let header = if size == 0 {
            let header = Header::new_database();
            if wal.is_some() {
                // The header is committed through the log like any page, so a
                // crash during creation leaves an empty file, not a broken one.
                header_dirty = true;
            } else {
                // Write the header now so the file is valid at once.
                storage.write_at(0, &header.encode()[..])?;
                storage.sync()?;
                tracer.event(&Event::PageWrite { page: 0 });
            }
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
            header_dirty,
            cache: Cache::new(cache_frames),
            tracer,
            wal,
            recovered,
        })
    }
}

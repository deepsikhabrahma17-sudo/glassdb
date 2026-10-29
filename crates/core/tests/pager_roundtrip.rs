//! Done-when check for the pager: 1,000 pages are written through a cache much
//! smaller than the data, the file is closed and reopened, and every byte
//! reads back identical.

use std::path::Path;
use std::sync::Arc;

use glassdb_core::{FileStorage, NoopTracer, PAGE_SIZE, Page, PageId, Pager};

const PAGES: u32 = 1000;
/// Small on purpose, so most pages are evicted and written back before close.
const FRAMES: usize = 16;

/// Deterministic pseudo-random contents for page `id` (xorshift64). Every page
/// differs, so a page written to the wrong place would be caught.
fn pattern(id: PageId) -> Box<Page> {
    let mut state = 0x9E37_79B9_7F4A_7C15u64 ^ u64::from(id);
    let mut page = Box::new([0u8; PAGE_SIZE]);
    for chunk in page.as_chunks_mut::<8>().0 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *chunk = state.to_le_bytes();
    }
    page
}

fn open(path: &Path) -> Pager {
    let storage = FileStorage::open(path).unwrap();
    Pager::open(Box::new(storage), FRAMES, Arc::new(NoopTracer)).unwrap()
}

/// Create a database at `path` holding pages 1..=PAGES, each filled with `pattern`.
fn write_pages(path: &Path) -> Pager {
    let mut pager = open(path);
    for expected in 1..=PAGES {
        let id = pager.allocate().unwrap();
        assert_eq!(id, expected);
        pager.write_page(id, &pattern(id)).unwrap();
    }
    pager
}

#[test]
fn thousand_pages_survive_close_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.db");
    write_pages(&path).close().unwrap();

    // On disk: the header plus 1,000 pages, with page n at byte n × 4096.
    let raw = std::fs::read(&path).unwrap();
    assert_eq!(raw.len(), (PAGES as usize + 1) * PAGE_SIZE);
    for id in 1..=PAGES {
        let start = id as usize * PAGE_SIZE;
        assert!(
            raw[start..start + PAGE_SIZE] == pattern(id)[..],
            "raw bytes of page {id} differ"
        );
    }

    let mut pager = open(&path);
    assert_eq!(pager.page_count(), PAGES + 1);
    for id in 1..=PAGES {
        assert!(
            pager.read_page(id).unwrap() == pattern(id),
            "page {id} differs after reopen"
        );
    }
}

#[test]
fn freed_pages_and_survivors_come_back_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("freelist.db");
    let mut pager = write_pages(&path);
    let freed: Vec<PageId> = (1..=PAGES).filter(|id| id % 3 == 0).collect();
    for &id in &freed {
        pager.free(id).unwrap();
    }
    pager.close().unwrap();

    let mut pager = open(&path);
    assert_eq!(pager.free_count(), freed.len() as u32);
    for id in (1..=PAGES).filter(|id| id % 3 != 0) {
        assert!(
            pager.read_page(id).unwrap() == pattern(id),
            "surviving page {id} differs after reopen"
        );
    }
    // The free list hands pages back newest-first, then the file grows.
    for &id in freed.iter().rev() {
        assert_eq!(pager.allocate().unwrap(), id);
    }
    assert_eq!(pager.free_count(), 0);
    assert_eq!(pager.allocate().unwrap(), PAGES + 1);
}

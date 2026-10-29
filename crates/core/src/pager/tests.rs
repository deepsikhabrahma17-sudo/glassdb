use std::sync::Arc;

use super::*;
use crate::storage::MemStorage;
use crate::trace::{Event, RecordingTracer};

/// Open a pager on `disk` with `frames` cache frames and a recording tracer.
fn open(disk: &MemStorage, frames: usize) -> (Pager, Arc<RecordingTracer>) {
    let tracer = Arc::new(RecordingTracer::default());
    let pager = Pager::open(Box::new(disk.clone()), frames, tracer.clone()).unwrap();
    (pager, tracer)
}

fn open_err(bytes: Vec<u8>) -> Result<Pager> {
    Pager::open(
        Box::new(MemStorage::from_bytes(bytes)),
        4,
        Arc::new(NoopTracer),
    )
}

fn filled(byte: u8) -> Page {
    [byte; PAGE_SIZE]
}

#[test]
fn new_database_is_just_a_header_page() {
    let disk = MemStorage::new();
    let (pager, tracer) = open(&disk, 4);
    assert_eq!(pager.page_count(), 1);
    assert_eq!(pager.free_count(), 0);
    let bytes = disk.to_bytes();
    assert_eq!(bytes.len(), PAGE_SIZE);
    assert_eq!(&bytes[..8], b"GLASSDB\0");
    assert_eq!(tracer.take(), vec![Event::PageWrite { page: 0 }]);
}

#[test]
fn allocate_hands_out_zeroed_pages_in_order() {
    let (mut pager, _) = open(&MemStorage::new(), 4);
    assert_eq!(pager.allocate().unwrap(), 1);
    assert_eq!(pager.allocate().unwrap(), 2);
    assert_eq!(pager.allocate().unwrap(), 3);
    assert_eq!(pager.page_count(), 4);
    assert_eq!(*pager.read_page(2).unwrap(), [0; PAGE_SIZE]);
}

#[test]
fn write_then_read_is_a_cache_hit() {
    let (mut pager, tracer) = open(&MemStorage::new(), 4);
    let id = pager.allocate().unwrap();
    pager.write_page(id, &filled(7)).unwrap();
    tracer.take();
    assert_eq!(*pager.read_page(id).unwrap(), filled(7));
    assert_eq!(tracer.take(), vec![Event::CacheHit { page: id }]);
}

#[test]
fn full_cache_evicts_least_recently_used_and_writes_it_back() {
    let (mut pager, tracer) = open(&MemStorage::new(), 2);
    for byte in 1..=3 {
        let id = pager.allocate().unwrap();
        pager.write_page(id, &filled(byte)).unwrap();
    }
    // Two frames: pages 2 and 3 are cached; page 1 was evicted to make room for 3.
    tracer.take();
    assert_eq!(*pager.read_page(1).unwrap(), filled(1));
    assert_eq!(
        tracer.take(),
        vec![
            Event::CacheMiss { page: 1 },
            Event::CacheEvict {
                page: 2,
                dirty: true
            },
            Event::PageWrite { page: 2 },
            Event::PageRead { page: 1 },
        ]
    );
    assert_eq!(*pager.read_page(2).unwrap(), filled(2));
}

#[test]
fn freed_page_is_reused_first_and_comes_back_zeroed() {
    let (mut pager, _) = open(&MemStorage::new(), 4);
    let a = pager.allocate().unwrap();
    let b = pager.allocate().unwrap();
    pager.write_page(b, &filled(9)).unwrap();
    pager.free(a).unwrap();
    pager.free(b).unwrap();
    assert_eq!(pager.free_count(), 2);

    assert_eq!(pager.allocate().unwrap(), b);
    assert_eq!(*pager.read_page(b).unwrap(), [0; PAGE_SIZE]);
    assert_eq!(pager.allocate().unwrap(), a);
    assert_eq!(pager.free_count(), 0);
    // Free list empty again: the file grows.
    assert_eq!(pager.allocate().unwrap(), 3);
}

#[test]
fn free_list_survives_reopen() {
    let disk = MemStorage::new();
    let (mut pager, _) = open(&disk, 4);
    for _ in 0..5 {
        pager.allocate().unwrap();
    }
    pager.free(2).unwrap();
    pager.free(4).unwrap();
    pager.close().unwrap();

    let (mut pager, _) = open(&disk, 4);
    assert_eq!(pager.page_count(), 6);
    assert_eq!(pager.free_count(), 2);
    assert_eq!(pager.allocate().unwrap(), 4);
    assert_eq!(pager.allocate().unwrap(), 2);
    assert_eq!(pager.allocate().unwrap(), 6);
}

#[test]
fn flush_writes_dirty_pages_in_order_then_the_header() {
    let (mut pager, tracer) = open(&MemStorage::new(), 8);
    for _ in 0..3 {
        pager.allocate().unwrap();
    }
    tracer.take();
    pager.flush().unwrap();
    assert_eq!(
        tracer.take(),
        vec![
            Event::PageWrite { page: 1 },
            Event::PageWrite { page: 2 },
            Event::PageWrite { page: 3 },
            Event::PageWrite { page: 0 },
        ]
    );
    // Nothing is left dirty, so a second flush writes nothing.
    pager.flush().unwrap();
    assert_eq!(tracer.take(), Vec::new());
}

#[test]
fn header_page_and_pages_past_the_end_are_out_of_range() {
    let (mut pager, _) = open(&MemStorage::new(), 4);
    pager.allocate().unwrap();
    assert!(matches!(pager.read_page(0), Err(Error::PageOutOfRange(0))));
    assert!(matches!(pager.read_page(2), Err(Error::PageOutOfRange(2))));
    assert!(matches!(
        pager.write_page(0, &filled(1)),
        Err(Error::PageOutOfRange(0))
    ));
    assert!(matches!(pager.free(0), Err(Error::PageOutOfRange(0))));
}

#[test]
fn garbage_file_is_rejected() {
    assert!(matches!(
        open_err(vec![0xAB; PAGE_SIZE]),
        Err(Error::BadMagic)
    ));
}

#[test]
fn short_file_without_magic_is_not_a_database() {
    // e.g. opening a small text file by mistake
    assert!(matches!(open_err(vec![0; 100]), Err(Error::BadMagic)));
    assert!(matches!(open_err(b"GLASS".to_vec()), Err(Error::BadMagic)));
}

#[test]
fn database_cut_short_inside_the_header_is_corrupt() {
    let mut bytes = b"GLASSDB\0".to_vec();
    bytes.resize(100, 0);
    assert!(matches!(open_err(bytes), Err(Error::Corrupt(_))));
}

#[test]
fn file_shorter_than_its_page_count_is_corrupt() {
    let disk = MemStorage::new();
    let (mut pager, _) = open(&disk, 4);
    for _ in 0..3 {
        pager.allocate().unwrap();
    }
    pager.close().unwrap();

    let mut bytes = disk.to_bytes();
    bytes.truncate(2 * PAGE_SIZE);
    assert!(matches!(open_err(bytes), Err(Error::Corrupt(_))));
}

//! `Pager::rollback`: pages, the header and the free list go back to the last
//! commit, wherever the transaction's changes had got to.

use std::sync::Arc;

use super::*;
use crate::storage::MemStorage;
use crate::trace::{Event, NoopTracer, RecordingTracer};

fn filled(byte: u8) -> Page {
    [byte; PAGE_SIZE]
}

fn open(db: &MemStorage, wal: &MemStorage, frames: usize) -> (Pager, Arc<RecordingTracer>) {
    let tracer = Arc::new(RecordingTracer::default());
    let pager = Pager::open_with_wal(
        Box::new(db.clone()),
        Box::new(wal.clone()),
        frames,
        tracer.clone(),
    )
    .unwrap();
    (pager, tracer)
}

/// A committed database holding pages 1..=n, each filled with its own id.
fn committed(n: u8, frames: usize) -> (MemStorage, MemStorage, Pager, Arc<RecordingTracer>) {
    let (db, wal) = (MemStorage::new(), MemStorage::new());
    let (mut pager, tracer) = open(&db, &wal, frames);
    for byte in 1..=n {
        let id = pager.allocate().unwrap();
        pager.write_page(id, &filled(byte)).unwrap();
    }
    pager.flush().unwrap();
    tracer.take();
    (db, wal, pager, tracer)
}

#[test]
fn rollback_restores_a_page_still_in_the_cache() {
    let (_, _, mut pager, _) = committed(2, 8);
    pager.write_page(1, &filled(9)).unwrap();
    assert_eq!(pager.rollback().unwrap(), 0, "nothing reached the log");
    assert_eq!(*pager.read_page(1).unwrap(), filled(1));
}

#[test]
fn rollback_restores_pages_that_spilled_to_the_log() {
    let (_, _, mut pager, _) = committed(6, 2);
    for id in 1..=6 {
        pager.write_page(id, &filled(50)).unwrap();
    }
    assert!(pager.rollback().unwrap() > 0, "a 2-frame cache must spill");
    for id in 1..=6 {
        assert_eq!(*pager.read_page(id).unwrap(), filled(id as u8));
    }
}

#[test]
fn rollback_forgets_a_clean_cached_copy_of_an_uncommitted_page() {
    // Page 1 spills to the log, then is read back. The cached copy is now
    // clean, but it holds uncommitted bytes, so it mustn't survive either.
    let (_, _, mut pager, _) = committed(3, 2);
    pager.write_page(1, &filled(50)).unwrap();
    pager.read_page(2).unwrap();
    pager.read_page(3).unwrap();
    assert_eq!(*pager.read_page(1).unwrap(), filled(50));
    pager.rollback().unwrap();
    assert_eq!(*pager.read_page(1).unwrap(), filled(1));
}

#[test]
fn rollback_undoes_allocation() {
    let (_, _, mut pager, _) = committed(2, 8);
    let id = pager.allocate().unwrap();
    assert_eq!(pager.page_count(), 4);
    pager.rollback().unwrap();
    assert_eq!(pager.page_count(), 3);
    assert!(matches!(
        pager.read_page(id),
        Err(Error::PageOutOfRange(page)) if page == id
    ));
    assert_eq!(pager.allocate().unwrap(), id, "the id is handed out again");
    assert_eq!(*pager.read_page(id).unwrap(), [0; PAGE_SIZE]);
}

#[test]
fn rollback_undoes_freeing() {
    let (_, _, mut pager, _) = committed(3, 8);
    pager.free(2).unwrap();
    pager.free(3).unwrap();
    pager.rollback().unwrap();
    assert_eq!(pager.free_count(), 0);
    pager.check_free_list().unwrap();
    assert_eq!(*pager.read_page(2).unwrap(), filled(2));
    assert_eq!(pager.allocate().unwrap(), 4, "the free list is empty again");
}

#[test]
fn rollback_keeps_the_free_list_as_it_was_committed() {
    let (_, _, mut pager, _) = committed(4, 8);
    pager.free(2).unwrap();
    pager.flush().unwrap();

    assert_eq!(pager.allocate().unwrap(), 2);
    pager.free(3).unwrap();
    pager.rollback().unwrap();
    assert_eq!(pager.free_count(), 1);
    pager.check_free_list().unwrap();
    assert_eq!(pager.allocate().unwrap(), 2);
}

#[test]
fn a_commit_after_a_rollback_keeps_only_the_commit() {
    let (db, wal, mut pager, _) = committed(2, 8);
    pager.write_page(1, &filled(9)).unwrap();
    pager.rollback().unwrap();
    pager.write_page(2, &filled(7)).unwrap();
    pager.flush().unwrap();
    drop(pager); // a crash

    let (mut pager, _) = open(&db, &wal, 8);
    assert_eq!(*pager.read_page(1).unwrap(), filled(1));
    assert_eq!(*pager.read_page(2).unwrap(), filled(7));
}

#[test]
fn a_crash_after_a_rollback_keeps_what_was_committed() {
    let (db, wal, mut pager, _) = committed(4, 2);
    for id in 1..=4 {
        pager.write_page(id, &filled(50)).unwrap();
    }
    pager.allocate().unwrap();
    pager.rollback().unwrap();
    drop(pager);

    let (mut pager, _) = open(&db, &wal, 2);
    assert_eq!(pager.recovered(), 1, "only the first commit is in the log");
    assert_eq!(pager.page_count(), 5);
    for id in 1..=4 {
        assert_eq!(*pager.read_page(id).unwrap(), filled(id as u8));
    }
}

#[test]
fn rollback_reports_the_frames_it_threw_away() {
    let (_, _, mut pager, tracer) = committed(3, 1);
    pager.write_page(1, &filled(9)).unwrap();
    pager.write_page(2, &filled(9)).unwrap();
    tracer.take();
    assert_eq!(pager.rollback().unwrap(), 1, "page 2 is still in the cache");
    assert_eq!(tracer.take(), vec![Event::TxnRollback { frames: 1 }]);
}

#[test]
fn rollback_needs_a_log() {
    let mut pager = Pager::open(Box::new(MemStorage::new()), 4, Arc::new(NoopTracer)).unwrap();
    assert!(pager.rollback().is_err());
}

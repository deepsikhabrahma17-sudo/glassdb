//! The pager with a write-ahead log: `flush` commits to the log, and the
//! main file only changes during a checkpoint.

use std::sync::Arc;

use super::*;
use crate::storage::MemStorage;
use crate::trace::{NoopTracer, RecordingTracer};

struct Disks {
    db: MemStorage,
    wal: MemStorage,
}

impl Disks {
    fn new() -> Disks {
        Disks {
            db: MemStorage::new(),
            wal: MemStorage::new(),
        }
    }

    fn open(&self, frames: usize) -> (Pager, Arc<RecordingTracer>) {
        let tracer = Arc::new(RecordingTracer::default());
        let pager = Pager::open_with_wal(
            Box::new(self.db.clone()),
            Box::new(self.wal.clone()),
            frames,
            tracer.clone(),
        )
        .unwrap();
        (pager, tracer)
    }
}

fn filled(byte: u8) -> Page {
    [byte; PAGE_SIZE]
}

/// Allocate `n` pages filled with 1, 2, 3, ...
fn write_pages(pager: &mut Pager, n: u8) {
    for byte in 1..=n {
        let id = pager.allocate().unwrap();
        pager.write_page(id, &filled(byte)).unwrap();
    }
}

#[test]
fn the_main_file_is_untouched_until_a_checkpoint() {
    let disks = Disks::new();
    let (mut pager, _) = disks.open(8);
    write_pages(&mut pager, 3);
    pager.flush().unwrap();
    assert!(
        disks.db.to_bytes().is_empty(),
        "committed work stays in the log"
    );
    assert!(!disks.wal.to_bytes().is_empty());

    pager.checkpoint().unwrap();
    assert!(
        disks.wal.to_bytes().is_empty(),
        "a checkpoint empties the log"
    );
    let bytes = disks.db.to_bytes();
    assert_eq!(bytes.len(), 4 * PAGE_SIZE);
    assert_eq!(&bytes[..8], b"GLASSDB\0");
    assert_eq!(bytes[2 * PAGE_SIZE..3 * PAGE_SIZE], filled(2));
}

#[test]
fn a_commit_appends_each_page_then_the_commit_record() {
    let disks = Disks::new();
    let (mut pager, tracer) = disks.open(8);
    write_pages(&mut pager, 2);
    tracer.take();
    pager.flush().unwrap();
    assert_eq!(
        tracer.take(),
        vec![
            Event::WalAppend { page: 1 },
            Event::WalAppend { page: 2 },
            Event::WalAppend { page: 0 },
            Event::WalCommit { frames: 3 },
        ]
    );
    // Nothing changed since, so a second flush writes nothing.
    let log = disks.wal.to_bytes();
    pager.flush().unwrap();
    assert_eq!(disks.wal.to_bytes(), log);
    assert!(tracer.take().is_empty());
}

#[test]
fn committed_pages_read_back_from_the_log_after_eviction() {
    let disks = Disks::new();
    let (mut pager, tracer) = disks.open(2);
    write_pages(&mut pager, 5);
    pager.flush().unwrap();
    tracer.take();
    assert_eq!(*pager.read_page(1).unwrap(), filled(1));
    let events = tracer.take();
    assert!(events.contains(&Event::WalRead { page: 1 }), "{events:?}");
    assert!(!events.contains(&Event::PageRead { page: 1 }), "{events:?}");
}

#[test]
fn a_dirty_page_evicted_mid_transaction_goes_to_the_log_and_reads_back() {
    let disks = Disks::new();
    let (mut pager, tracer) = disks.open(2);
    write_pages(&mut pager, 5);
    let events = tracer.take();
    assert!(events.contains(&Event::WalAppend { page: 1 }), "{events:?}");
    assert!(disks.db.to_bytes().is_empty(), "never into the main file");
    for byte in 1..=5u8 {
        assert_eq!(*pager.read_page(u32::from(byte)).unwrap(), filled(byte));
    }
    pager.flush().unwrap();
    for byte in 1..=5u8 {
        assert_eq!(*pager.read_page(u32::from(byte)).unwrap(), filled(byte));
    }
}

#[test]
fn committed_work_survives_a_crash_and_uncommitted_work_vanishes() {
    let disks = Disks::new();
    let (mut pager, _) = disks.open(2);
    write_pages(&mut pager, 3);
    pager.flush().unwrap();
    // More work, some of it spilled to the log, but no commit. Then a crash.
    pager.write_page(1, &filled(99)).unwrap();
    write_pages(&mut pager, 4);
    drop(pager);

    let (mut pager, tracer) = disks.open(2);
    let events = tracer.take();
    assert_eq!(pager.recovered(), 1);
    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::WalRecovered { transactions: 1, frames: 4, discarded } if *discarded > 0
        )),
        "{events:?}"
    );
    assert_eq!(pager.page_count(), 4);
    for byte in 1..=3u8 {
        assert_eq!(*pager.read_page(u32::from(byte)).unwrap(), filled(byte));
    }
    assert!(disks.wal.to_bytes().is_empty(), "recovery checkpoints");
    assert_eq!(disks.db.to_bytes().len(), 4 * PAGE_SIZE);
}

#[test]
fn free_list_changes_commit_with_the_data() {
    let disks = Disks::new();
    let (mut pager, _) = disks.open(8);
    write_pages(&mut pager, 3);
    pager.flush().unwrap();
    pager.free(2).unwrap();
    pager.flush().unwrap();
    drop(pager);

    let (mut pager, _) = disks.open(8);
    assert_eq!(pager.free_count(), 1);
    assert_eq!(pager.allocate().unwrap(), 2);
}

#[test]
fn a_crash_before_the_first_commit_leaves_a_fresh_database() {
    let disks = Disks::new();
    let (mut pager, _) = disks.open(8);
    write_pages(&mut pager, 3);
    drop(pager);

    assert!(disks.db.to_bytes().is_empty());
    let (pager, _) = disks.open(8);
    assert_eq!(pager.page_count(), 1);
    assert_eq!(pager.recovered(), 0);
}

#[test]
fn close_checkpoints_everything() {
    let disks = Disks::new();
    let (mut pager, _) = disks.open(8);
    write_pages(&mut pager, 2);
    pager.close().unwrap();
    assert!(disks.wal.to_bytes().is_empty());
    assert_eq!(disks.db.to_bytes().len(), 3 * PAGE_SIZE);
    let (mut pager, _) = disks.open(8);
    assert_eq!(pager.recovered(), 0);
    assert_eq!(*pager.read_page(2).unwrap(), filled(2));
}

#[test]
fn a_long_log_checkpoints_itself() {
    let disks = Disks::new();
    let (mut pager, tracer) = disks.open(8);
    let id = pager.allocate().unwrap();
    let mut commits = 0;
    while !tracer
        .take()
        .iter()
        .any(|e| matches!(e, Event::Checkpoint { .. }))
    {
        pager.write_page(id, &filled(7)).unwrap();
        pager.flush().unwrap();
        commits += 1;
        assert!(commits <= CHECKPOINT_FRAMES, "never checkpointed");
    }
    assert!(disks.wal.to_bytes().is_empty());
    assert_eq!(disks.db.to_bytes().len(), 2 * PAGE_SIZE);
}

#[test]
fn a_database_from_before_the_log_opens_normally() {
    let disks = Disks::new();
    let mut pager = Pager::open(Box::new(disks.db.clone()), 8, Arc::new(NoopTracer)).unwrap();
    write_pages(&mut pager, 2);
    pager.close().unwrap();

    let (mut pager, _) = disks.open(8);
    assert_eq!(pager.page_count(), 3);
    assert_eq!(*pager.read_page(2).unwrap(), filled(2));
}

//! `Wal::rollback`: the running transaction's frames leave no trace.

use super::*;
use crate::pager::PAGE_SIZE;
use crate::storage::MemStorage;

fn page(byte: u8) -> Page {
    [byte; PAGE_SIZE]
}

fn open(disk: &MemStorage) -> (Wal, Recovery) {
    Wal::open(Box::new(disk.clone())).unwrap()
}

fn read(wal: &mut Wal, id: PageId) -> Option<u8> {
    let mut buf = page(0);
    wal.read(id, &mut buf).unwrap().then_some(buf[0])
}

#[test]
fn rollback_forgets_the_running_transaction() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(2)).unwrap();
    wal.append_page(2, &page(3)).unwrap();
    assert_eq!(
        read(&mut wal, 1),
        Some(2),
        "the transaction sees its own page"
    );

    assert_eq!(wal.rollback().unwrap(), 2);
    assert_eq!(read(&mut wal, 1), Some(1));
    assert_eq!(read(&mut wal, 2), None);
    assert_eq!(wal.pages(), vec![1]);
    assert_eq!(wal.frames(), 1);
}

#[test]
fn rollback_cuts_the_file_back_to_the_last_commit() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    let committed = disk.to_bytes();

    wal.append_page(2, &page(2)).unwrap();
    wal.append_page(3, &page(3)).unwrap();
    wal.rollback().unwrap();
    assert_eq!(disk.to_bytes(), committed);
}

#[test]
fn rollback_with_no_commits_leaves_an_empty_log() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    assert_eq!(wal.rollback().unwrap(), 1);
    assert!(disk.to_bytes().is_empty());

    // The next transaction starts the file again, header included.
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();
    let (mut reopened, recovery) = open(&disk);
    assert_eq!(recovery.transactions, 1);
    assert_eq!(read(&mut reopened, 2), Some(2));
}

#[test]
fn rollback_with_nothing_to_undo_changes_nothing() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    assert_eq!(wal.rollback().unwrap(), 0);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    let before = disk.to_bytes();
    assert_eq!(wal.rollback().unwrap(), 0);
    assert_eq!(disk.to_bytes(), before);
    assert_eq!(wal.frames(), 1);
}

#[test]
fn the_checksum_chain_continues_from_the_last_commit() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(9)).unwrap();
    wal.rollback().unwrap();
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();

    let (mut reopened, recovery) = open(&disk);
    assert_eq!(recovery.transactions, 2, "the second commit must chain");
    assert_eq!(recovery.discarded, 0);
    assert_eq!(read(&mut reopened, 1), Some(1));
    assert_eq!(read(&mut reopened, 2), Some(2));
}

#[test]
fn rollback_works_on_a_log_that_was_reopened() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();

    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(2)).unwrap();
    wal.rollback().unwrap();
    wal.append_page(2, &page(3)).unwrap();
    wal.commit().unwrap();

    let (mut reopened, recovery) = open(&disk);
    assert_eq!(recovery.transactions, 2);
    assert_eq!(read(&mut reopened, 1), Some(1));
}

#[test]
fn frames_that_outlive_their_rollback_are_never_recovered() {
    // A rollback doesn't sync, so a crash can bring the cut frames back.
    // They have no commit record, so recovery must still throw them away.
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(2)).unwrap();
    wal.append_page(2, &page(3)).unwrap();
    let with_ghosts = disk.to_bytes();
    wal.rollback().unwrap();

    let (mut reopened, recovery) = open(&MemStorage::from_bytes(with_ghosts));
    assert_eq!(recovery.transactions, 1);
    assert_eq!(recovery.discarded, 2);
    assert_eq!(read(&mut reopened, 1), Some(1));
    assert_eq!(read(&mut reopened, 2), None);
}

#[test]
fn a_reset_also_forgets_the_rollback_point() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.reset().unwrap();

    wal.append_page(2, &page(2)).unwrap();
    wal.rollback().unwrap();
    assert!(disk.to_bytes().is_empty());
}

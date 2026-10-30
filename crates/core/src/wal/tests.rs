use crc32fast::Hasher;

use super::*;
use crate::error::Error;
use crate::pager::{PAGE_SIZE, Page};
use crate::storage::MemStorage;

fn page(byte: u8) -> Page {
    [byte; PAGE_SIZE]
}

fn open(disk: &MemStorage) -> (Wal, Recovery) {
    Wal::open(Box::new(disk.clone())).unwrap()
}

/// Page `id` as the log sees it, or `None` if the log doesn't hold it.
fn read(wal: &mut Wal, id: PageId) -> Option<u8> {
    let mut buf = page(0);
    wal.read(id, &mut buf).unwrap().then_some(buf[0])
}

/// Size of a log holding one transaction of `frames` page frames.
fn txn_len(frames: usize) -> usize {
    frames * FRAME_SIZE + RECORD_HEADER
}

#[test]
fn byte_layout_matches_the_design_doc() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(7, &page(0xAB)).unwrap();
    assert_eq!(wal.commit().unwrap(), 1);

    let bytes = disk.to_bytes();
    assert_eq!(bytes.len(), 16 + 12 + PAGE_SIZE + 12);
    // File header.
    assert_eq!(&bytes[0..8], b"GLASSWAL");
    assert_eq!(bytes[8..12], 1u32.to_le_bytes());
    assert_eq!(bytes[12..16], [0; 4]);
    // Page frame: kind, three zero bytes, page id, checksum, page.
    assert_eq!(bytes[16], 1);
    assert_eq!(bytes[17..20], [0; 3]);
    assert_eq!(bytes[20..24], 7u32.to_le_bytes());
    let mut crc = Hasher::new_with_initial(0);
    crc.update(&bytes[16..24]);
    crc.update(&bytes[28..28 + PAGE_SIZE]);
    let frame_crc = crc.finalize();
    assert_eq!(bytes[24..28], frame_crc.to_le_bytes());
    assert_eq!(bytes[28..28 + PAGE_SIZE], page(0xAB));
    // Commit record: kind, frame count, checksum chained from the frame's.
    let at = 28 + PAGE_SIZE;
    assert_eq!(bytes[at], 2);
    assert_eq!(bytes[at + 4..at + 8], 1u32.to_le_bytes());
    let mut crc = Hasher::new_with_initial(frame_crc);
    crc.update(&bytes[at..at + 8]);
    assert_eq!(bytes[at + 8..at + 12], crc.finalize().to_le_bytes());
}

#[test]
fn committed_pages_are_recovered() {
    let disk = MemStorage::new();
    let (mut wal, recovery) = open(&disk);
    assert_eq!(recovery, Recovery::default());
    wal.append_page(1, &page(1)).unwrap();
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();
    wal.append_page(3, &page(3)).unwrap();
    wal.commit().unwrap();
    assert_eq!(wal.frames(), 3);

    let (mut wal, recovery) = open(&disk);
    assert_eq!(
        recovery,
        Recovery {
            transactions: 2,
            frames: 3,
            discarded: 0
        }
    );
    assert_eq!(wal.pages(), vec![1, 2, 3]);
    assert_eq!(read(&mut wal, 2), Some(2));
    assert_eq!(read(&mut wal, 9), None);
}

#[test]
fn the_newest_frame_of_a_page_wins() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.append_page(1, &page(2)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(3)).unwrap();
    wal.commit().unwrap();
    assert_eq!(read(&mut wal, 1), Some(3));

    let (mut wal, _) = open(&disk);
    assert_eq!(read(&mut wal, 1), Some(3));
}

#[test]
fn a_running_transaction_reads_its_own_frames() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(2)).unwrap();
    assert_eq!(read(&mut wal, 1), Some(2));
}

#[test]
fn uncommitted_frames_are_discarded() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(2)).unwrap();
    wal.append_page(2, &page(2)).unwrap();
    drop(wal);

    let (mut wal, recovery) = open(&disk);
    assert_eq!(
        recovery,
        Recovery {
            transactions: 1,
            frames: 1,
            discarded: 2
        }
    );
    assert_eq!(read(&mut wal, 1), Some(1));
    assert_eq!(read(&mut wal, 2), None);
}

#[test]
fn a_log_cut_at_any_byte_keeps_only_whole_transactions() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(1, &page(2)).unwrap();
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();
    let full = disk.to_bytes();
    let first_txn_end = HEADER_SIZE as usize + txn_len(1);

    // Every length up to the first commit record recovers nothing.
    for len in (0..first_txn_end)
        .step_by(97)
        .chain(first_txn_end - 12..first_txn_end)
    {
        let cut = MemStorage::from_bytes(full[..len].to_vec());
        let (mut wal, recovery) = open(&cut);
        assert_eq!(recovery.transactions, 0, "cut at {len}");
        assert_eq!(read(&mut wal, 1), None, "cut at {len}");
    }
    // Every length after it, until the last byte, recovers exactly the first.
    for len in first_txn_end..full.len() {
        let cut = MemStorage::from_bytes(full[..len].to_vec());
        let (mut wal, recovery) = open(&cut);
        assert_eq!(recovery.transactions, 1, "cut at {len}");
        assert_eq!(read(&mut wal, 1), Some(1), "cut at {len}");
        assert_eq!(read(&mut wal, 2), None, "cut at {len}");
    }
    assert_eq!(open(&MemStorage::from_bytes(full)).1.transactions, 2);
}

#[test]
fn a_flipped_bit_stops_recovery_there() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();
    let full = disk.to_bytes();
    let second_txn = HEADER_SIZE as usize + txn_len(1);

    // Damage in the second transaction loses only that transaction.
    let mut bytes = full.clone();
    bytes[second_txn + 100] ^= 1;
    assert_eq!(open(&MemStorage::from_bytes(bytes)).1.transactions, 1);

    // Damage in the first stops there: the valid second one can't be trusted.
    let mut bytes = full.clone();
    bytes[HEADER_SIZE as usize + 100] ^= 1;
    assert_eq!(open(&MemStorage::from_bytes(bytes)).1.transactions, 0);
}

#[test]
fn frames_in_the_wrong_order_are_rejected() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();

    // Each frame's own bytes are intact, but the chained checksum notices.
    let mut bytes = disk.to_bytes();
    let (a, b) = (HEADER_SIZE as usize, HEADER_SIZE as usize + FRAME_SIZE);
    let first = bytes[a..b].to_vec();
    let second = bytes[b..b + FRAME_SIZE].to_vec();
    bytes[a..b].copy_from_slice(&second);
    bytes[b..b + FRAME_SIZE].copy_from_slice(&first);
    assert_eq!(open(&MemStorage::from_bytes(bytes)).1.transactions, 0);
}

#[test]
fn a_damaged_or_foreign_header_means_an_empty_log() {
    for bytes in [vec![1, 2, 3], vec![0; 5000]] {
        let (mut wal, recovery) = open(&MemStorage::from_bytes(bytes));
        assert_eq!(recovery.transactions, 0);
        assert_eq!(read(&mut wal, 1), None);
    }
}

#[test]
fn a_header_torn_at_any_byte_means_an_empty_log() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    let full = disk.to_bytes();

    // Only the first `keep` bytes of the header landed, but the later writes
    // did, so the rest of the header reads back as zeros. (The bytes after
    // byte 8 are zero anyway, so keeping 9 or more is a complete header.)
    for keep in 0..9 {
        let mut bytes = full.clone();
        bytes[keep..9].fill(0);
        let (mut wal, recovery) = open(&MemStorage::from_bytes(bytes));
        assert_eq!(recovery.transactions, 0, "header cut at {keep}");
        assert_eq!(read(&mut wal, 1), None, "header cut at {keep}");
    }
}

#[test]
fn a_newer_log_version_is_refused() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    let mut bytes = disk.to_bytes();
    bytes[8] = 9;
    let result = Wal::open(Box::new(MemStorage::from_bytes(bytes)));
    assert!(matches!(result, Err(Error::UnsupportedVersion(9))));
}

#[test]
fn reset_empties_the_log_and_it_can_be_used_again() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    wal.append_page(1, &page(1)).unwrap();
    wal.commit().unwrap();
    wal.reset().unwrap();
    assert!(disk.to_bytes().is_empty());
    assert_eq!(wal.frames(), 0);
    assert_eq!(read(&mut wal, 1), None);

    wal.append_page(2, &page(2)).unwrap();
    wal.commit().unwrap();
    let (mut wal, recovery) = open(&disk);
    assert_eq!(recovery.transactions, 1);
    assert_eq!(read(&mut wal, 1), None);
    assert_eq!(read(&mut wal, 2), Some(2));
}

#[test]
fn committing_nothing_writes_nothing() {
    let disk = MemStorage::new();
    let (mut wal, _) = open(&disk);
    assert_eq!(wal.commit().unwrap(), 0);
    assert!(disk.to_bytes().is_empty());
}

use std::ops::Bound;
use std::sync::Arc;

use proptest::prelude::*;

use super::*;
use crate::error::Error;
use crate::pager::Pager;
use crate::planner::KeyRange;
use crate::storage::MemStorage;
use crate::trace::NoopTracer;

fn pager() -> Pager {
    Pager::open(Box::new(MemStorage::new()), 32, Arc::new(NoopTracer)).unwrap()
}

/// The commit log's tree is at page 2 in a real database, with page 1 before it.
fn log(pager: &mut Pager) -> CommitLog {
    pager.allocate().unwrap();
    CommitLog::create(pager).unwrap()
}

#[test]
fn stamps_round_trip() {
    let value = stamped(0x0102_0304_0506_0708, b"row");
    assert_eq!(value.len(), TXN_BYTES + 3);
    assert_eq!(
        unstamped(&value).unwrap(),
        (0x0102_0304_0506_0708, &b"row"[..])
    );
    assert!(matches!(unstamped(&[1, 2]), Err(Error::Corrupt(_))));
}

#[test]
fn a_history_key_splits_back_into_its_parts() {
    let key = history_key(-5, 9);
    assert_eq!(key.len(), 16);
    assert_eq!(split_history_key(&key).unwrap(), (-5, 9));
    assert!(matches!(split_history_key(&[1]), Err(Error::Corrupt(_))));
}

#[test]
fn history_keys_sort_by_row_then_age() {
    let mut keys = [
        history_key(2, 1),
        history_key(1, 300),
        history_key(-1, 7),
        history_key(1, 2),
    ];
    keys.sort();
    let order: Vec<_> = keys.iter().map(|k| split_history_key(k).unwrap()).collect();
    assert_eq!(order, [(-1, 7), (1, 2), (1, 300), (2, 1)]);
}

#[test]
fn visibility_is_created_at_or_before_and_not_yet_deleted() {
    // Created by txn 3 and replaced by txn 7.
    let old = |at| visible(3, Some(7), at);
    assert!(!old(2), "before it was made");
    assert!(old(3), "the transaction that made it");
    assert!(old(6));
    assert!(!old(7), "the transaction that replaced it");
    assert!(!old(8));
    // The current version of a row.
    assert!(!visible(3, None, 2));
    assert!(visible(3, None, 3));
    assert!(visible(3, None, u64::MAX));
}

/// A table key range, and which rows of a history tree it must find.
fn history_range(start: Bound<i64>, end: Bound<i64>) -> KeyRange {
    let key = |b: Bound<i64>| b.map(|id| crate::types::encode_key(id).to_vec());
    history_bounds(&KeyRange {
        start: key(start),
        end: key(end),
    })
}

#[test]
fn a_table_range_covers_every_version_of_its_rows() {
    let keys: Vec<Vec<u8>> = (1..=3)
        .flat_map(|id| [history_key(id, 0), history_key(id, u64::MAX)])
        .collect();
    let found = |range: KeyRange| -> Vec<i64> {
        let mut ids: Vec<i64> = keys
            .iter()
            .filter(|key| {
                let key = key.as_slice();
                let after = match &range.start {
                    Bound::Unbounded => true,
                    Bound::Included(s) => key >= s.as_slice(),
                    Bound::Excluded(s) => key > s.as_slice(),
                };
                let before = match &range.end {
                    Bound::Unbounded => true,
                    Bound::Included(e) => key <= e.as_slice(),
                    Bound::Excluded(e) => key < e.as_slice(),
                };
                after && before
            })
            .map(|key| split_history_key(key).unwrap().0)
            .collect();
        ids.dedup();
        ids
    };
    use Bound::*;
    assert_eq!(found(history_range(Included(2), Included(2))), [2]);
    assert_eq!(found(history_range(Excluded(1), Unbounded)), [2, 3]);
    assert_eq!(found(history_range(Included(2), Unbounded)), [2, 3]);
    assert_eq!(found(history_range(Unbounded, Excluded(3))), [1, 2]);
    assert_eq!(found(history_range(Unbounded, Included(2))), [1, 2]);
    assert_eq!(found(history_range(Excluded(1), Excluded(3))), [2]);
    assert_eq!(found(history_range(Unbounded, Unbounded)), [1, 2, 3]);
}

#[test]
fn the_commit_log_remembers_each_commit() {
    let mut pager = pager();
    let log = log(&mut pager);
    assert_eq!(log.last(&mut pager).unwrap(), None);
    log.record(&mut pager, 1, 100).unwrap();
    log.record(&mut pager, 2, 200).unwrap();
    assert_eq!(
        log.last(&mut pager).unwrap(),
        Some(Commit { txn: 2, time: 200 })
    );
    assert_eq!(log.time_of(&mut pager, 1).unwrap(), Some(100));
    assert_eq!(log.time_of(&mut pager, 3).unwrap(), None);
}

#[test]
fn a_time_finds_the_last_commit_at_or_before_it() {
    let mut pager = pager();
    let log = log(&mut pager);
    // Txns 2 and 3 share a time, as commits in one microsecond can.
    for (txn, time) in [(1, 10), (2, 20), (3, 20), (4, 40)] {
        log.record(&mut pager, txn, time).unwrap();
    }
    let at = |pager: &mut Pager, time| log.txn_at(pager, time, 4).unwrap();
    assert_eq!(at(&mut pager, 9), 0, "before the first commit");
    assert_eq!(at(&mut pager, 10), 1, "exactly at a commit");
    assert_eq!(at(&mut pager, 19), 1);
    assert_eq!(at(&mut pager, 20), 3, "the last of a tie");
    assert_eq!(at(&mut pager, 39), 3);
    assert_eq!(at(&mut pager, 40), 4);
    assert_eq!(at(&mut pager, 1_000), 4, "after the last commit");
}

#[test]
fn recent_lists_commits_newest_first() {
    let mut pager = pager();
    let log = log(&mut pager);
    for txn in 1..=5 {
        log.record(&mut pager, txn, txn as i64 * 10).unwrap();
    }
    let txns: Vec<TxnId> = log
        .recent(&mut pager, 2)
        .unwrap()
        .iter()
        .map(|c| c.txn)
        .collect();
    assert_eq!(txns, [5, 4]);
    assert_eq!(log.recent(&mut pager, 99).unwrap().len(), 5);
    assert!(log.recent(&mut pager, 0).unwrap().is_empty());
}

#[test]
fn check_accepts_a_good_log() {
    let mut pager = pager();
    let log = log(&mut pager);
    for txn in 1..=500 {
        log.record(&mut pager, txn, 7).unwrap();
    }
    assert!(log.check(&mut pager, 500).unwrap() >= 1);
}

#[test]
fn check_refuses_a_gap_in_the_ids() {
    let mut pager = pager();
    let log = log(&mut pager);
    log.record(&mut pager, 1, 1).unwrap();
    log.record(&mut pager, 3, 2).unwrap();
    assert!(matches!(
        log.check(&mut pager, 3),
        Err(Error::Corrupt(msg)) if msg.contains("txn 3")
    ));
}

#[test]
fn check_refuses_a_time_that_goes_backwards() {
    let mut pager = pager();
    let log = log(&mut pager);
    log.record(&mut pager, 1, 5).unwrap();
    log.record(&mut pager, 2, 4).unwrap();
    assert!(matches!(
        log.check(&mut pager, 2),
        Err(Error::Corrupt(msg)) if msg.contains("backwards")
    ));
}

#[test]
fn check_refuses_a_log_shorter_than_expected() {
    let mut pager = pager();
    let log = log(&mut pager);
    log.record(&mut pager, 1, 5).unwrap();
    assert!(matches!(log.check(&mut pager, 2), Err(Error::Corrupt(_))));
}

proptest! {
    #[test]
    fn binary_search_agrees_with_a_walk(
        gaps in proptest::collection::vec(0i64..4, 1..80),
        probe in -1i64..250,
    ) {
        let mut pager = pager();
        let log = log(&mut pager);
        let mut time = 0;
        let mut times = Vec::new();
        for (i, gap) in gaps.iter().enumerate() {
            time += gap;
            times.push(time);
            log.record(&mut pager, i as TxnId + 1, time).unwrap();
        }
        let walked = times.iter().filter(|&&t| t <= probe).count() as TxnId;
        let found = log.txn_at(&mut pager, probe, times.len() as TxnId).unwrap();
        prop_assert_eq!(found, walked);
    }
}

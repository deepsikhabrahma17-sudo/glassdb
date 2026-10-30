//! Scenario tests: each one drives the tree into one specific shape change
//! and checks both the data and the structural events it reports.

use std::ops::Bound;
use std::sync::Arc;

use super::*;
use crate::error::Error;
use crate::pager::Pager;
use crate::storage::MemStorage;
use crate::trace::{Event, RecordingTracer};

fn setup() -> (Pager, BTree, Arc<RecordingTracer>) {
    let tracer = Arc::new(RecordingTracer::default());
    let mut pager = Pager::open(Box::new(MemStorage::new()), 32, tracer.clone()).unwrap();
    let tree = BTree::create(&mut pager).unwrap();
    tracer.take();
    (pager, tree, tracer)
}

/// Key `k0007`-style: fixed width, so byte order is numeric order.
fn key(i: u32) -> Vec<u8> {
    format!("k{i:04}").into_bytes()
}

/// 900-byte value: each leaf entry is then 911 bytes, so a leaf holds
/// exactly 4 and the 5th insert splits it.
fn big(fill: u8) -> Vec<u8> {
    vec![fill; 900]
}

/// Only the events that change the tree's shape.
fn shape_events(tracer: &RecordingTracer) -> Vec<Event> {
    tracer
        .take()
        .into_iter()
        .filter(|e| {
            matches!(
                e,
                Event::NodeSplit { .. }
                    | Event::NodeMerge { .. }
                    | Event::NodeBorrow { .. }
                    | Event::RootGrow { .. }
                    | Event::RootShrink { .. }
            )
        })
        .collect()
}

fn keys_of(
    pager: &mut Pager,
    tree: &BTree,
    start: Bound<&[u8]>,
    end: Bound<&[u8]>,
) -> Vec<Vec<u8>> {
    tree.range(pager, start, end)
        .unwrap()
        .map(|entry| entry.unwrap().0)
        .collect()
}

#[test]
fn empty_tree() {
    let (mut pager, tree, _) = setup();
    assert_eq!(tree.get(&mut pager, b"anything").unwrap(), None);
    assert_eq!(tree.delete(&mut pager, b"anything").unwrap(), None);
    assert_eq!(tree.iter(&mut pager).unwrap().count(), 0);
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.height, stats.leaves, stats.entries), (1, 1, 0));
}

#[test]
fn insert_get_overwrite_delete() {
    let (mut pager, tree, _) = setup();
    assert_eq!(tree.insert(&mut pager, b"b", b"2").unwrap(), None);
    assert_eq!(tree.insert(&mut pager, b"a", b"1").unwrap(), None);
    assert_eq!(tree.get(&mut pager, b"a").unwrap(), Some(b"1".to_vec()));
    // Inserting an existing key replaces the value and returns the old one.
    assert_eq!(
        tree.insert(&mut pager, b"a", b"one").unwrap(),
        Some(b"1".to_vec())
    );
    assert_eq!(tree.get(&mut pager, b"a").unwrap(), Some(b"one".to_vec()));
    assert_eq!(
        tree.delete(&mut pager, b"a").unwrap(),
        Some(b"one".to_vec())
    );
    assert_eq!(tree.get(&mut pager, b"a").unwrap(), None);
    assert_eq!(tree.get(&mut pager, b"b").unwrap(), Some(b"2".to_vec()));
    tree.check(&mut pager).unwrap();
}

#[test]
fn first_leaf_split_grows_the_root_in_place() {
    let (mut pager, tree, tracer) = setup();
    for i in 0..4 {
        tree.insert(&mut pager, &key(i), &big(1)).unwrap();
    }
    assert!(
        shape_events(&tracer).is_empty(),
        "4 entries still fit one leaf"
    );

    tree.insert(&mut pager, &key(4), &big(1)).unwrap();
    // The root (page 1) splits: right half to new page 2, then the left half
    // moves to page 3 and page 1 becomes their parent.
    assert_eq!(
        shape_events(&tracer),
        vec![
            Event::NodeSplit {
                page: 1,
                new_page: 2,
                separator: key(2)
            },
            Event::RootGrow {
                root: 1,
                left: 3,
                right: 2
            },
        ]
    );
    assert_eq!(tree.root(), 1);
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.height, stats.leaves, stats.entries), (2, 2, 5));
}

#[test]
fn internal_nodes_split_too() {
    let (mut pager, tree, tracer) = setup();
    // 500-byte keys: an internal node holds only 8, so it overflows quickly.
    let long_key = |i: u32| {
        let mut k = format!("{i:04}").into_bytes();
        k.resize(500, b'.');
        k
    };
    for i in 0..60 {
        tree.insert(&mut pager, &long_key(i), &[7; 400]).unwrap();
    }
    let grows = shape_events(&tracer)
        .iter()
        .filter(|e| matches!(e, Event::RootGrow { .. }))
        .count();
    assert_eq!(grows, 2, "leaf root split, then internal root split");
    assert_eq!(tree.check(&mut pager).unwrap().height, 3);
    for i in 0..60 {
        assert_eq!(
            tree.get(&mut pager, &long_key(i)).unwrap(),
            Some(vec![7; 400])
        );
    }
}

#[test]
fn delete_merges_leaves_and_shrinks_the_root() {
    let (mut pager, tree, tracer) = setup();
    for i in 0..5 {
        tree.insert(&mut pager, &key(i), &big(1)).unwrap();
    }
    // Leaves: page 3 = [k0, k1], page 2 = [k2, k3, k4].
    tracer.take();
    tree.delete(&mut pager, &key(2)).unwrap();
    assert!(
        shape_events(&tracer).is_empty(),
        "two entries are still enough"
    );

    // Page 2 drops to one entry (underfull); both fit in one page, so they merge.
    // The root is left with a single child and absorbs it.
    tree.delete(&mut pager, &key(3)).unwrap();
    assert_eq!(
        shape_events(&tracer),
        vec![
            Event::NodeMerge { left: 3, right: 2 },
            Event::RootShrink { root: 1, child: 3 },
        ]
    );
    assert_eq!(
        pager.free_count(),
        2,
        "pages 2 and 3 are back on the free list"
    );
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.height, stats.entries), (1, 3));
}

#[test]
fn delete_borrows_when_neighbours_are_too_full_to_merge() {
    let (mut pager, tree, tracer) = setup();
    for i in 0..5 {
        tree.insert(&mut pager, &key(i), &big(1)).unwrap();
    }
    // Fill the left leaf back to 4 entries: [k0, k0a, k1, k1a].
    tree.insert(&mut pager, b"k0000a", &big(2)).unwrap();
    tree.insert(&mut pager, b"k0001a", &big(2)).unwrap();
    tree.delete(&mut pager, &key(3)).unwrap();
    tracer.take();

    // Right leaf drops to [k2]: 5 entries won't fit one page, so share them.
    tree.delete(&mut pager, &key(4)).unwrap();
    assert_eq!(
        shape_events(&tracer),
        vec![Event::NodeBorrow { left: 3, right: 2 }]
    );
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.leaves, stats.entries), (2, 5));
}

#[test]
fn deleting_everything_frees_every_page() {
    let (mut pager, tree, _) = setup();
    let n = 300;
    for i in 0..n {
        let len = (i as usize * 37) % 900;
        tree.insert(&mut pager, &key(i), &vec![i as u8; len])
            .unwrap();
    }
    assert!(tree.check(&mut pager).unwrap().height >= 2);

    // Delete in a scrambled order (multiplying by 7 permutes 0..300).
    for i in 0..n {
        let k = key((i * 7) % n);
        assert!(tree.delete(&mut pager, &k).unwrap().is_some(), "{k:?}");
    }
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.height, stats.leaves, stats.entries), (1, 1, 0));
    // Only the header and the root page are still in use.
    assert_eq!(pager.free_count(), pager.page_count() - 2);
}

#[test]
fn range_scans_follow_the_leaf_chain() {
    let (mut pager, tree, _) = setup();
    for i in 0..100 {
        tree.insert(&mut pager, &key(i), &big(i as u8)).unwrap();
    }
    assert!(tree.check(&mut pager).unwrap().leaves > 20);
    let keys = |a: u32, b: u32| (a..b).map(key).collect::<Vec<_>>();
    let (k10, k20) = (key(10), key(20));
    use Bound::{Excluded, Included, Unbounded};

    assert_eq!(
        keys_of(&mut pager, &tree, Unbounded, Unbounded),
        keys(0, 100)
    );
    assert_eq!(
        keys_of(&mut pager, &tree, Included(&k10), Excluded(&k20)),
        keys(10, 20)
    );
    assert_eq!(
        keys_of(&mut pager, &tree, Excluded(&k10), Included(&k20)),
        keys(11, 21)
    );
    assert_eq!(
        keys_of(&mut pager, &tree, Included(&k20), Unbounded),
        keys(20, 100)
    );
    assert_eq!(
        keys_of(&mut pager, &tree, Unbounded, Excluded(&k10)),
        keys(0, 10)
    );
    // A start between two keys begins at the next key.
    assert_eq!(
        keys_of(&mut pager, &tree, Included(b"k0010x"), Excluded(&k20)),
        keys(11, 20)
    );
    // Backwards and empty ranges return nothing.
    assert!(keys_of(&mut pager, &tree, Included(&k20), Excluded(&k10)).is_empty());
    assert!(keys_of(&mut pager, &tree, Included(&k10), Excluded(&k10)).is_empty());
    // Values come along with their keys.
    let first = tree
        .range(&mut pager, Included(&k20), Unbounded)
        .unwrap()
        .next();
    assert_eq!(first.unwrap().unwrap(), (key(20), big(20)));
}

#[test]
fn rejects_oversized_entries() {
    let (mut pager, tree, _) = setup();
    let too_long_key = vec![b'k'; MAX_KEY + 1];
    assert!(matches!(
        tree.insert(&mut pager, &too_long_key, b""),
        Err(Error::EntryTooLarge {
            key_len: 513,
            value_len: 0
        })
    ));
    assert!(matches!(
        tree.insert(&mut pager, b"k", &vec![0; MAX_ENTRY]),
        Err(Error::EntryTooLarge { .. })
    ));
    // Exactly at the limits is fine.
    let max_key = vec![b'k'; MAX_KEY];
    tree.insert(&mut pager, &max_key, &vec![0; MAX_ENTRY - MAX_KEY])
        .unwrap();
    tree.check(&mut pager).unwrap();
}

#[test]
fn empty_key_is_a_valid_key() {
    let (mut pager, tree, _) = setup();
    tree.insert(&mut pager, b"b", b"2").unwrap();
    tree.insert(&mut pager, b"", b"empty").unwrap();
    assert_eq!(tree.get(&mut pager, b"").unwrap(), Some(b"empty".to_vec()));
    let first = tree.iter(&mut pager).unwrap().next().unwrap().unwrap();
    assert_eq!(first, (Vec::new(), b"empty".to_vec()));
}

#[test]
fn dump_shows_the_tree() {
    let (mut pager, tree, _) = setup();
    for i in 0..5 {
        tree.insert(&mut pager, &key(i), &big(1)).unwrap();
    }
    let dump = tree.dump(&mut pager).unwrap();
    assert_eq!(
        dump,
        "page 1 (root) internal: k0002\n  \
         page 3 leaf: k0000 k0001 → page 2\n  \
         page 2 leaf: k0002 k0003 k0004 → end\n"
    );
}

#[test]
fn last_returns_the_largest_entry() {
    let (mut pager, tree, _) = setup();
    assert_eq!(tree.last(&mut pager).unwrap(), None);
    for i in (0..50).rev() {
        tree.insert(&mut pager, &key(i), &big(i as u8)).unwrap();
    }
    assert!(tree.check(&mut pager).unwrap().height >= 2);
    assert_eq!(tree.last(&mut pager).unwrap(), Some((key(49), big(49))));
}

#[test]
fn destroy_frees_every_page_including_the_root() {
    let (mut pager, tree, _) = setup();
    for i in 0..100 {
        tree.insert(&mut pager, &key(i), &big(1)).unwrap();
    }
    tree.destroy(&mut pager).unwrap();
    // Only the header page is still in use.
    assert_eq!(pager.free_count(), pager.page_count() - 1);
}

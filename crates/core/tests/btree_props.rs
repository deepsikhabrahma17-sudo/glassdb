//! Done-when check for the B+ tree: random inserts, deletes, lookups and
//! range scans must always agree with Rust's `BTreeMap`.

use std::collections::BTreeMap;
use std::ops::Bound;
use std::sync::Arc;

use glassdb_core::{BTree, MemStorage, NoopTracer, Pager};
use proptest::prelude::*;

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

/// A small cache, so pages are evicted and re-read all the time.
const FRAMES: usize = 32;

fn new_tree() -> (Pager, BTree) {
    let mut pager = Pager::open(Box::new(MemStorage::new()), FRAMES, Arc::new(NoopTracer)).unwrap();
    let tree = BTree::create(&mut pager).unwrap();
    (pager, tree)
}

/// Every entry in key order.
fn scan(pager: &mut Pager, tree: &BTree) -> Vec<(Vec<u8>, Vec<u8>)> {
    tree.iter(pager).unwrap().collect::<Result<_, _>>().unwrap()
}

// ---------------------------------------------------------------------------
// 1. Random operation sequences, shrunk to the smallest failure by proptest.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Op {
    Insert(Vec<u8>, Vec<u8>),
    Delete(Vec<u8>),
    Get(Vec<u8>),
    Range(Vec<u8>, Vec<u8>),
}

fn key() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        // Short keys from a 4-letter alphabet: the same keys come up again
        // and again, so overwrites and deletes of existing keys are common.
        3 => prop::collection::vec(0u8..4, 0..6),
        // Long random keys make nodes fill and split after few entries.
        1 => prop::collection::vec(any::<u8>(), 0..300),
    ]
}

fn value() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..600)
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (key(), value()).prop_map(|(k, v)| Op::Insert(k, v)),
        3 => key().prop_map(Op::Delete),
        1 => key().prop_map(Op::Get),
        1 => (key(), key()).prop_map(|(a, b)| Op::Range(a, b)),
    ]
}

fn run_ops(ops: &[Op]) {
    let (mut pager, tree) = new_tree();
    let mut model = Model::new();
    for op in ops {
        match op {
            Op::Insert(k, v) => {
                let got = tree.insert(&mut pager, k, v).unwrap();
                assert_eq!(got, model.insert(k.clone(), v.clone()), "insert {k:?}");
            }
            Op::Delete(k) => {
                let got = tree.delete(&mut pager, k).unwrap();
                assert_eq!(got, model.remove(k), "delete {k:?}");
            }
            Op::Get(k) => {
                assert_eq!(
                    tree.get(&mut pager, k).unwrap().as_ref(),
                    model.get(k),
                    "get {k:?}"
                );
            }
            Op::Range(a, b) => {
                let got: Vec<_> = tree
                    .range(&mut pager, Bound::Included(a), Bound::Excluded(b))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap();
                // BTreeMap::range panics on a backwards range; ours is just empty.
                let want: Vec<_> = if a <= b {
                    model
                        .range::<[u8], _>((Bound::Included(&a[..]), Bound::Excluded(&b[..])))
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                } else {
                    Vec::new()
                };
                assert_eq!(got, want, "range {a:?}..{b:?}");
            }
        }
    }
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!(stats.entries, model.len());
    let want: Vec<_> = model.into_iter().collect();
    assert_eq!(scan(&mut pager, &tree), want);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn random_operations_match_btreemap(ops in prop::collection::vec(op(), 1..2000)) {
        run_ops(&ops);
    }
}

// ---------------------------------------------------------------------------
// 2. 100,000 random inserts and deletes per seed, checked as they happen.
// ---------------------------------------------------------------------------

/// xorshift64: a tiny deterministic random number generator.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const OPS: usize = 100_000;
const KEY_SPACE: u64 = 20_000;

/// Keys of varied length that don't sort in numeric order.
fn big_key(n: u64) -> Vec<u8> {
    let mut key = format!("key-{n}").into_bytes();
    key.resize(key.len() + (n % 50) as usize, b'-');
    key
}

fn hundred_thousand_ops(seed: u64) {
    let mut rng = Rng(seed | 1); // xorshift must not start at 0
    let (mut pager, tree) = new_tree();
    let mut model = Model::new();

    for step in 1..=OPS {
        let key = big_key(rng.below(KEY_SPACE));
        if rng.below(10) < 6 {
            let len = rng.below(200) as usize;
            let value: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
            let got = tree.insert(&mut pager, &key, &value).unwrap();
            assert_eq!(got, model.insert(key.clone(), value), "step {step}: insert");
        } else {
            let got = tree.delete(&mut pager, &key).unwrap();
            assert_eq!(got, model.remove(&key), "step {step}: delete");
        }
        assert_eq!(
            tree.get(&mut pager, &key).unwrap().as_ref(),
            model.get(&key),
            "step {step}: get"
        );
        if step % 5_000 == 0 {
            let stats = tree.check(&mut pager).unwrap();
            assert_eq!(stats.entries, model.len(), "step {step}: entry count");
        }
    }
    let want: Vec<_> = model.clone().into_iter().collect();
    assert_eq!(scan(&mut pager, &tree), want, "full scan after {OPS} ops");

    // Empty the tree again: it must shrink to one leaf and leak no pages.
    for key in model.keys() {
        assert!(tree.delete(&mut pager, key).unwrap().is_some());
    }
    let stats = tree.check(&mut pager).unwrap();
    assert_eq!((stats.height, stats.entries), (1, 0));
    assert_eq!(pager.free_count(), pager.page_count() - 2, "leaked pages");
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(4))]

    #[test]
    fn hundred_thousand_random_ops_match_btreemap(seed in any::<u64>()) {
        hundred_thousand_ops(seed);
    }
}

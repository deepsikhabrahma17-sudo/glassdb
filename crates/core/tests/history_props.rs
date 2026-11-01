//! The Phase 7 done-when check for time travel: every past state comes back
//! exactly.
//!
//! Random statements run against the engine and a plain model. At each commit
//! the model's whole state is saved, so afterwards every past transaction can
//! be compared, by id and by time, through `AS OF` and through `HISTORY`.
//! Readers pinned along the way must keep answering from the moment they
//! pinned, and everything must survive reopening the database.

mod history_model;

use history_model::{NAMES, Op, Run};
use proptest::prelude::*;

fn op() -> impl Strategy<Value = Op> {
    let table = 0..NAMES.len();
    prop_oneof![
        8 => (table.clone(), 0..12i64, 0..50i64).prop_map(|(table, id, n)| Op::Insert { table, id, n }),
        6 => (table.clone(), 0..12i64, 0..4i64, 0..50i64)
            .prop_map(|(table, low, span, n)| Op::Update { table, low, span, n }),
        4 => (table.clone(), 0..12i64, 0..3i64)
            .prop_map(|(table, low, span)| Op::Delete { table, low, span }),
        2 => (table.clone(), 0..12i64).prop_map(|(table, id)| Op::MoveKey { table, id }),
        3 => Just(Op::Begin),
        2 => Just(Op::Commit),
        1 => Just(Op::Rollback),
        1 => table.clone().prop_map(|table| Op::CreateIndex { table }),
        1 => table.clone().prop_map(|table| Op::Create { table }),
        1 => table.prop_map(|table| Op::Drop { table }),
        1 => (0..12u64).prop_map(|age| Op::Vacuum { age }),
        2 => Just(Op::Pin),
        1 => Just(Op::Unpin),
        1 => Just(Op::Reopen),
        1 => Just(Op::Checkpoint),
    ]
}

fn run_all(ops: &[Op]) {
    let dir = tempfile::tempdir().unwrap();
    let mut run = Run::new(dir.path().join("t.db"));
    for op in ops {
        run.apply(op);
        run.check_readers();
    }
    run.finish();
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn every_past_state_comes_back_exactly(ops in proptest::collection::vec(op(), 1..120)) {
        run_all(&ops);
    }
}

#[test]
fn a_long_run_stays_exact() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::{Config, RngAlgorithm, TestRng, TestRunner};
    let rng = TestRng::deterministic_rng(RngAlgorithm::ChaCha);
    let mut runner = TestRunner::new_with_rng(Config::default(), rng);
    let ops = proptest::collection::vec(op(), 1500)
        .new_tree(&mut runner)
        .unwrap()
        .current();
    run_all(&ops);
}

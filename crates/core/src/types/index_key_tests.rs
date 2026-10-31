//! Index keys must sort as bytes the way their values sort as values.

use std::cmp::Ordering;

use proptest::prelude::*;

use super::index_key::*;
use super::*;

fn key(values: &[Value]) -> Vec<u8> {
    index_key(values, 0)
}

/// Assert that `values` is in strictly ascending key order.
fn ascending(values: &[Value]) {
    for pair in values.windows(2) {
        assert!(
            key(&pair[..1]) < key(&pair[1..]),
            "{:?} should sort before {:?}",
            pair[0],
            pair[1]
        );
    }
}

#[test]
fn null_sorts_before_every_value() {
    for value in [
        Value::Bool(false),
        Value::Int(i64::MIN),
        Value::Real(f64::NEG_INFINITY),
        Value::Text(String::new()),
    ] {
        assert!(key(&[Value::Null]) < key(&[value]));
    }
}

#[test]
fn numbers_sort_with_their_signs() {
    ascending(&[-5, -1, 0, 1, 5].map(Value::Int));
    ascending(&[i64::MIN, i64::MIN + 1, -1, 0, i64::MAX].map(Value::Int));
    ascending(
        &[
            f64::NEG_INFINITY,
            -1e300,
            -2.5,
            -1e-300,
            0.0,
            1e-300,
            2.5,
            1e300,
            f64::INFINITY,
        ]
        .map(Value::Real),
    );
}

#[test]
fn negative_zero_is_zero() {
    assert_eq!(key(&[Value::Real(-0.0)]), key(&[Value::Real(0.0)]));
}

#[test]
fn text_sorts_by_bytes_and_a_prefix_comes_first() {
    ascending(
        &[
            "", "\0", "\0\0", "\0a", "\u{1}", "a", "a\0", "a\0b", "ab", "b",
        ]
        .map(|s| Value::Text(s.to_string())),
    );
}

#[test]
fn a_shorter_text_stays_in_order_in_a_multi_column_key() {
    // Without an end marker, ('a', 'z') would sort after ('ab', 'a').
    let row = |a: &str, b: &str| key(&[Value::Text(a.into()), Value::Text(b.into())]);
    assert!(row("a", "z") < row("ab", "a"));
    assert!(row("a\0", "a") > row("a", "z"));
}

#[test]
fn booleans_sort_false_first() {
    ascending(&[Value::Bool(false), Value::Bool(true)]);
}

#[test]
fn the_row_id_is_the_last_eight_bytes() {
    let values = [Value::Text("x".into()), Value::Int(3)];
    for id in [i64::MIN, -1, 0, 7, i64::MAX] {
        let key = index_key(&values, id);
        assert_eq!(rowid_of(&key).unwrap(), encode_key(id));
    }
    assert!(rowid_of(b"short").is_err());
}

#[test]
fn equal_values_sort_by_row_id() {
    let values = [Value::Int(1)];
    assert!(index_key(&values, -1) < index_key(&values, 1));
    assert!(index_key(&values, 1) < index_key(&values, 2));
}

#[test]
fn the_successor_comes_after_everything_with_the_prefix() {
    assert_eq!(successor(&[1, 2]), Some(vec![1, 3]));
    assert_eq!(successor(&[1, 0xFF]), Some(vec![2]));
    assert_eq!(successor(&[0xFF, 0xFF]), None);
    assert_eq!(successor(&[]), None);

    let prefix = key(&[Value::Int(5)]);
    let prefix = &prefix[..prefix.len() - 8];
    let next = successor(prefix).unwrap();
    let inside = index_key(&[Value::Int(5)], i64::MAX);
    let outside = index_key(&[Value::Int(6)], i64::MIN);
    assert!(prefix < inside.as_slice() && inside < next && next < outside);
}

fn real() -> impl Strategy<Value = f64> {
    prop_oneof![
        any::<f64>().prop_filter("no NaN in a column", |r| !r.is_nan()),
        Just(0.0),
        Just(-0.0),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
    ]
}

/// Mostly short text over a small alphabet, so prefixes and zeros are common.
fn text() -> impl Strategy<Value = String> {
    prop::collection::vec(
        prop_oneof![Just('\0'), Just('\u{1}'), Just('a'), Just('é')],
        0..5,
    )
    .prop_map(|chars| chars.into_iter().collect())
}

/// A value of the column type numbered `kind`, or NULL.
fn value(kind: u8) -> BoxedStrategy<Value> {
    let inner = match kind % 4 {
        0 => any::<i64>().prop_map(Value::Int).boxed(),
        1 => real().prop_map(Value::Real).boxed(),
        2 => text().prop_map(Value::Text).boxed(),
        _ => any::<bool>().prop_map(Value::Bool).boxed(),
    };
    prop_oneof![1 => Just(Value::Null), 6 => inner].boxed()
}

/// Two rows of the same column types, at most three columns wide.
fn row_pair() -> impl Strategy<Value = (Vec<Value>, Vec<Value>)> {
    prop::collection::vec(any::<u8>(), 1..4).prop_flat_map(|kinds| {
        let row: Vec<_> = kinds.iter().map(|k| value(*k)).collect();
        (row.clone(), row)
    })
}

fn row_order(a: &[Value], b: &[Value]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        let ordering = x.sort_cmp(y);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn key_order_is_value_order((a, b) in row_pair()) {
        // Equal values tie, and the row id then decides.
        prop_assert_eq!(key(&a).cmp(&key(&b)), row_order(&a, &b), "{:?} {:?}", a, b);
    }
}

use std::cmp::Ordering;

use proptest::prelude::*;

use super::*;
use crate::error::Error;

#[test]
fn values_print_like_the_cli_shows_them() {
    assert_eq!(Value::Int(-3).to_string(), "-3");
    assert_eq!(Value::Real(2.0).to_string(), "2.0");
    assert_eq!(Value::Real(0.5).to_string(), "0.5");
    assert_eq!(Value::Text("hi".into()).to_string(), "hi");
    assert_eq!(Value::Bool(true).to_string(), "true");
    assert_eq!(Value::Null.to_string(), "NULL");
    // As SQL literals, for error messages and printed statements.
    assert_eq!(Value::Text("it's".into()).to_sql(), "'it''s'");
    assert_eq!(Value::Bool(false).to_sql(), "FALSE");
    assert_eq!(Value::Real(3.0).to_sql(), "3.0");
}

#[test]
fn compare_mixes_numbers_and_knows_null_is_unknown() {
    assert_eq!(
        Value::Int(1).compare(&Value::Real(1.5)).unwrap(),
        Some(Ordering::Less)
    );
    assert_eq!(
        Value::Real(2.0).compare(&Value::Int(2)).unwrap(),
        Some(Ordering::Equal)
    );
    assert_eq!(
        Value::Text("b".into())
            .compare(&Value::Text("abc".into()))
            .unwrap(),
        Some(Ordering::Greater)
    );
    assert_eq!(Value::Null.compare(&Value::Int(1)).unwrap(), None);
    assert!(matches!(
        Value::Int(1).compare(&Value::Text("1".into())),
        Err(Error::Query(msg)) if msg == "cannot compare INT and TEXT"
    ));
}

#[test]
fn sort_order_puts_null_first() {
    let mut values = vec![Value::Int(3), Value::Null, Value::Real(1.5), Value::Int(-1)];
    values.sort_by(Value::sort_cmp);
    assert_eq!(
        values,
        vec![Value::Null, Value::Int(-1), Value::Real(1.5), Value::Int(3)]
    );
}

#[test]
fn coercion_into_column_types() {
    assert_eq!(Value::Int(3).coerce(DataType::Real), Some(Value::Real(3.0)));
    assert_eq!(Value::Int(3).coerce(DataType::Int), Some(Value::Int(3)));
    assert_eq!(Value::Null.coerce(DataType::Bool), Some(Value::Null));
    assert_eq!(Value::Real(1.0).coerce(DataType::Int), None);
    assert_eq!(Value::Text("1".into()).coerce(DataType::Int), None);
}

#[test]
fn row_byte_layout() {
    let row = [
        Value::Int(1),
        Value::Null,
        Value::Text("ab".into()),
        Value::Bool(true),
        Value::Real(0.5),
    ];
    let mut want = vec![5, 0]; // column count
    want.extend([1, 1, 0, 0, 0, 0, 0, 0, 0]); // INT 1
    want.push(0); // NULL
    want.extend([3, 2, 0, b'a', b'b']); // TEXT "ab"
    want.extend([4, 1]); // BOOL true
    want.push(2); // REAL 0.5
    want.extend(0.5f64.to_le_bytes());
    assert_eq!(encode_row(&row), want);
}

#[test]
fn damaged_rows_are_corrupt_not_a_panic() {
    for bytes in [
        &[1, 0, 9][..],
        &[1, 0, 1, 0, 0],
        &[2, 0, 0],
        &[1, 0, 3, 5, 0, b'a'],
        &[1],
    ] {
        assert!(
            matches!(decode_row(bytes), Err(Error::Corrupt(_))),
            "{bytes:?}"
        );
    }
    assert!(matches!(decode_key(&[1, 2, 3]), Err(Error::Corrupt(_))));
}

fn value() -> impl Strategy<Value = Value> {
    prop_oneof![
        Just(Value::Null),
        any::<i64>().prop_map(Value::Int),
        prop::num::f64::NORMAL.prop_map(Value::Real),
        ".{0,40}".prop_map(Value::Text),
        any::<bool>().prop_map(Value::Bool),
    ]
}

proptest! {
    #[test]
    fn rows_round_trip(row in prop::collection::vec(value(), 0..20)) {
        prop_assert_eq!(decode_row(&encode_row(&row)).unwrap(), row);
    }

    /// Table keys must sort in the B+ tree exactly like the integers do.
    #[test]
    fn key_bytes_sort_like_integers(a: i64, b: i64) {
        prop_assert_eq!(encode_key(a).cmp(&encode_key(b)), a.cmp(&b));
        prop_assert_eq!(decode_key(&encode_key(a)).unwrap(), a);
    }
}

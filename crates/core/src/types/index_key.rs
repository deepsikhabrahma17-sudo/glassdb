//! Index keys: column values written so that byte order equals value order.
//!
//! A B+ tree compares keys byte by byte, so comparing two index keys must give
//! the same answer as comparing their values, column by column. Each value is
//! a tag followed by bytes that sort correctly:
//!
//! | tag | type | data after the tag                                        |
//! |-----|------|-----------------------------------------------------------|
//! | 1   | NULL | nothing, so NULL sorts before every value                 |
//! | 2   | BOOL | 1 byte, 0 or 1                                            |
//! | 3   | INT  | 8 bytes, as in `encode_key`                               |
//! | 4   | REAL | 8 bytes, the IEEE bits arranged to sort like the numbers  |
//! | 5   | TEXT | the bytes, with 0 escaped as `0 255`, then the end `0 1`  |
//!
//! The end marker keeps a shorter text before a longer one that starts the
//! same, even with more columns after it, and sorts below every escaped byte.
//!
//! The row id comes last, as 8 bytes. It makes every key unique, so many rows
//! can share the same column values, and it says which row the entry is for.

use super::Value;
use super::encode::encode_key;
use crate::error::{Error, Result};

const TAG_NULL: u8 = 1;
const TAG_BOOL: u8 = 2;
const TAG_INT: u8 = 3;
const TAG_REAL: u8 = 4;
const TAG_TEXT: u8 = 5;

/// The smallest possible start of a value that isn't NULL. A range with only
/// an upper bound starts here, so it skips the NULLs.
pub const FIRST_VALUE: u8 = TAG_BOOL;

/// Append `value` to a key.
pub fn encode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(TAG_NULL),
        Value::Bool(b) => {
            out.push(TAG_BOOL);
            out.push(u8::from(*b));
        }
        Value::Int(n) => {
            out.push(TAG_INT);
            out.extend(encode_key(*n));
        }
        Value::Real(r) => {
            out.push(TAG_REAL);
            out.extend(real_bits(*r).to_be_bytes());
        }
        Value::Text(s) => {
            out.push(TAG_TEXT);
            for byte in s.bytes() {
                out.push(byte);
                if byte == 0 {
                    out.push(255);
                }
            }
            out.extend([0, 1]);
        }
    }
}

/// A float's bits as a number that sorts like the float. Positive floats
/// already sort by their bits, so setting the sign bit puts them above the
/// negatives. Negative floats sort backwards, so all their bits are flipped.
fn real_bits(r: f64) -> u64 {
    // Adding 0.0 turns -0.0 into 0.0, because the two compare equal.
    let bits = (r + 0.0).to_bits();
    if bits >> 63 == 1 {
        !bits
    } else {
        bits | 1 << 63
    }
}

/// The key of a row with these indexed `values`.
pub fn index_key(values: &[Value], rowid: i64) -> Vec<u8> {
    let mut key = Vec::with_capacity(values.len() * 10 + 8);
    for value in values {
        encode_value(value, &mut key);
    }
    key.extend(encode_key(rowid));
    key
}

/// The table key of the row an index key belongs to.
pub fn rowid_of(key: &[u8]) -> Result<[u8; 8]> {
    key.len()
        .checked_sub(8)
        .and_then(|at| key[at..].try_into().ok())
        .ok_or_else(|| Error::Corrupt(format!("index key is only {} bytes", key.len())))
}

/// The smallest byte string above everything that starts with `prefix`, or
/// `None` if there is nothing above it.
pub fn successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut next = prefix.to_vec();
    while let Some(last) = next.last_mut() {
        if *last < 255 {
            *last += 1;
            return Some(next);
        }
        next.pop();
    }
    None
}

//! Rows and keys as bytes.
//!
//! A row is a column count followed by each value as a type tag and data:
//!
//! | tag | type | data after the tag            |
//! |-----|------|-------------------------------|
//! | 0   | NULL | nothing                       |
//! | 1   | INT  | 8 bytes, little-endian i64    |
//! | 2   | REAL | 8 bytes, little-endian f64    |
//! | 3   | TEXT | u16 length, then UTF-8 bytes  |
//! | 4   | BOOL | 1 byte, 0 or 1                |
//!
//! A table key is a row id written so that byte order equals numeric order.

use super::Value;
use crate::error::{Error, Result};

const TAG_NULL: u8 = 0;
const TAG_INT: u8 = 1;
const TAG_REAL: u8 = 2;
const TAG_TEXT: u8 = 3;
const TAG_BOOL: u8 = 4;

pub fn encode_row(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + values.len() * 9);
    out.extend((values.len() as u16).to_le_bytes());
    for value in values {
        match value {
            Value::Null => out.push(TAG_NULL),
            Value::Int(n) => {
                out.push(TAG_INT);
                out.extend(n.to_le_bytes());
            }
            Value::Real(r) => {
                out.push(TAG_REAL);
                out.extend(r.to_le_bytes());
            }
            Value::Text(s) => {
                // Rows are capped well below 64 KB, so the length fits in a u16.
                out.push(TAG_TEXT);
                out.extend((s.len() as u16).to_le_bytes());
                out.extend(s.as_bytes());
            }
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(u8::from(*b));
            }
        }
    }
    out
}

pub fn decode_row(bytes: &[u8]) -> Result<Vec<Value>> {
    let mut reader = Reader { bytes, pos: 0 };
    let count = u16::from_le_bytes(reader.take()?) as usize;
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        let [tag] = reader.take()?;
        values.push(match tag {
            TAG_NULL => Value::Null,
            TAG_INT => Value::Int(i64::from_le_bytes(reader.take()?)),
            TAG_REAL => Value::Real(f64::from_le_bytes(reader.take()?)),
            TAG_TEXT => {
                let len = u16::from_le_bytes(reader.take()?) as usize;
                let text = reader.slice(len)?;
                Value::Text(
                    String::from_utf8(text.to_vec()).map_err(|_| corrupt("text is not UTF-8"))?,
                )
            }
            TAG_BOOL => match reader.take()? {
                [0] => Value::Bool(false),
                [1] => Value::Bool(true),
                [b] => return Err(corrupt(&format!("bad BOOL byte {b}"))),
            },
            other => return Err(corrupt(&format!("unknown type tag {other}"))),
        });
    }
    if reader.pos != bytes.len() {
        return Err(corrupt("extra bytes after the last column"));
    }
    Ok(values)
}

/// A row id as an 8-byte key. Flipping the sign bit and writing big-endian
/// makes the bytes sort like the numbers: -1 < 0 < 1 as byte strings too.
pub fn encode_key(rowid: i64) -> [u8; 8] {
    ((rowid as u64) ^ (1 << 63)).to_be_bytes()
}

pub fn decode_key(bytes: &[u8]) -> Result<i64> {
    let bytes: [u8; 8] = bytes
        .try_into()
        .map_err(|_| corrupt(&format!("row key is {} bytes, not 8", bytes.len())))?;
    Ok((u64::from_be_bytes(bytes) ^ (1 << 63)) as i64)
}

fn corrupt(msg: &str) -> Error {
    Error::Corrupt(format!("row: {msg}"))
}

/// Reads fixed-size pieces off the front of a byte slice.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn slice(&mut self, len: usize) -> Result<&'a [u8]> {
        let piece = self
            .bytes
            .get(self.pos..self.pos + len)
            .ok_or_else(|| corrupt("ends in the middle of a value"))?;
        self.pos += len;
        Ok(piece)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.slice(N)?.try_into().unwrap())
    }
}

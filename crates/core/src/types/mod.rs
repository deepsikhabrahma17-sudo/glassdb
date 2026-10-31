//! SQL values and column types.

mod encode;
mod index_key;

#[cfg(test)]
mod index_key_tests;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::fmt;

use crate::error::{Error, Result};
pub use encode::{decode_key, decode_row, encode_key, encode_row};
pub use index_key::{FIRST_VALUE, encode_value, index_key, rowid_of, successor};

/// The type of a table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    Int,
    Real,
    Text,
    Bool,
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            DataType::Int => "INT",
            DataType::Real => "REAL",
            DataType::Text => "TEXT",
            DataType::Bool => "BOOL",
        };
        write!(f, "{name}")
    }
}

/// One SQL value. `Null` means "unknown" and fits in a column of any type.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Bool(bool),
}

impl Value {
    /// The type's name, for error messages.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "NULL",
            Value::Int(_) => "INT",
            Value::Real(_) => "REAL",
            Value::Text(_) => "TEXT",
            Value::Bool(_) => "BOOL",
        }
    }

    /// The value written as a SQL literal: text quoted, booleans in capitals.
    pub fn to_sql(&self) -> String {
        match self {
            Value::Text(s) => format!("'{}'", s.replace('\'', "''")),
            Value::Bool(true) => "TRUE".into(),
            Value::Bool(false) => "FALSE".into(),
            other => other.to_string(),
        }
    }

    /// Compare two values the SQL way. `Ok(None)` means one of them is NULL,
    /// so the answer is unknown. INT and REAL compare as numbers; any other
    /// mix of types is an error.
    pub fn compare(&self, other: &Value) -> Result<Option<Ordering>> {
        use Value::*;
        Ok(match (self, other) {
            (Null, _) | (_, Null) => None,
            (Int(a), Int(b)) => Some(a.cmp(b)),
            (Int(_) | Real(_), Int(_) | Real(_)) => {
                // Neither side can be NaN: dividing by zero is an error.
                Some(
                    self.as_f64()
                        .partial_cmp(&other.as_f64())
                        .unwrap_or(Ordering::Equal),
                )
            }
            (Text(a), Text(b)) => Some(a.cmp(b)),
            (Bool(a), Bool(b)) => Some(a.cmp(b)),
            _ => {
                return Err(Error::Query(format!(
                    "cannot compare {} and {}",
                    self.type_name(),
                    other.type_name()
                )));
            }
        })
    }

    /// A total order for ORDER BY: NULL sorts first, then by value. Values of
    /// unrelated types (which a typed column never mixes) sort by type.
    pub fn sort_cmp(&self, other: &Value) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Bool(_) => 1,
                Value::Int(_) | Value::Real(_) => 2,
                Value::Text(_) => 3,
            }
        }
        match self.compare(other) {
            Ok(Some(ordering)) => ordering,
            _ => rank(self).cmp(&rank(other)),
        }
    }

    /// The value as stored in a column of type `ty`, or `None` if it doesn't
    /// fit. An INT is accepted into a REAL column and becomes a REAL.
    pub fn coerce(self, ty: DataType) -> Option<Value> {
        match (self, ty) {
            (Value::Null, _) => Some(Value::Null),
            (Value::Int(n), DataType::Int) => Some(Value::Int(n)),
            (Value::Int(n), DataType::Real) => Some(Value::Real(n as f64)),
            (Value::Real(r), DataType::Real) => Some(Value::Real(r)),
            (Value::Text(s), DataType::Text) => Some(Value::Text(s)),
            (Value::Bool(b), DataType::Bool) => Some(Value::Bool(b)),
            _ => None,
        }
    }

    fn as_f64(&self) -> f64 {
        match self {
            Value::Int(n) => *n as f64,
            Value::Real(r) => *r,
            _ => f64::NAN,
        }
    }
}

impl fmt::Display for Value {
    /// How results are shown: text without quotes, REALs always with a `.`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Int(n) => write!(f, "{n}"),
            Value::Real(r) => {
                // Rust prints 2.0 as "2"; add ".0" so a REAL never looks like an INT.
                let text = r.to_string();
                if text.contains(['.', 'e', 'i', 'N']) {
                    write!(f, "{text}")
                } else {
                    write!(f, "{text}.0")
                }
            }
            Value::Text(s) => write!(f, "{s}"),
            Value::Bool(b) => write!(f, "{b}"),
        }
    }
}

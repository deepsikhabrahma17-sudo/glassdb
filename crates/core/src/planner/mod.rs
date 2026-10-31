//! The planner: how to find the rows a WHERE clause wants.
//!
//! It splits WHERE at each top-level AND, keeps the conditions of the form
//! `column op constant`, and picks the best way to use them. It reads no
//! pages, so its choice depends only on the schema and the statement.
//!
//! **An index changes speed, never results.** Whatever way is picked, the
//! executor still applies the whole WHERE to every row it finds. So a key
//! range may hold rows that don't match, but must never miss a row that does,
//! and anything the planner isn't sure about is simply left to that filter.

#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::ops::Bound;

use crate::catalog::{IndexSchema, TableSchema};
use crate::exec::eval::{Scope, eval};
use crate::sql::{BinaryOp, Expr};
use crate::types::{DataType, FIRST_VALUE, Value, encode_key, encode_value, successor};

/// A range of B+ tree keys.
#[derive(Debug, Clone, PartialEq)]
pub struct KeyRange {
    pub start: Bound<Vec<u8>>,
    pub end: Bound<Vec<u8>>,
}

/// How to find a statement's rows. `conditions` say why, for `EXPLAIN`.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    FullScan,
    /// A range of the table's own tree. `lookup` means a single key.
    PrimaryKey {
        range: KeyRange,
        lookup: bool,
        conditions: Vec<String>,
    },
    /// A range of an index, whose entries name the rows to fetch.
    Index {
        index: IndexSchema,
        range: KeyRange,
        conditions: Vec<String>,
    },
}

impl Access {
    /// One line for `EXPLAIN`.
    pub fn describe(&self, table: &str) -> String {
        match self {
            Access::FullScan => format!("FULL SCAN {table}"),
            Access::PrimaryKey {
                lookup, conditions, ..
            } => {
                let kind = if *lookup { "LOOKUP" } else { "RANGE" };
                format!("PRIMARY KEY {kind} ON {table} ({})", conditions.join(", "))
            }
            Access::Index {
                index, conditions, ..
            } => format!(
                "INDEX SCAN {} ON {table} ({})",
                index.name,
                conditions.join(", ")
            ),
        }
    }
}

/// What WHERE says about one column.
#[derive(Default, Clone)]
struct Constraint {
    equal: Option<Value>,
    /// The value and whether it is included.
    lower: Option<(Value, bool)>,
    upper: Option<(Value, bool)>,
}

impl Constraint {
    fn add(&mut self, op: BinaryOp, value: Value) {
        match op {
            BinaryOp::Eq => {
                self.equal.get_or_insert(value);
            }
            BinaryOp::Gt | BinaryOp::GtEq => {
                let candidate = (value, op == BinaryOp::GtEq);
                tighten(&mut self.lower, candidate, Ordering::Greater);
            }
            _ => {
                let candidate = (value, op == BinaryOp::LtEq);
                tighten(&mut self.upper, candidate, Ordering::Less);
            }
        }
    }

    fn has_range(&self) -> bool {
        self.lower.is_some() || self.upper.is_some()
    }

    /// The conditions on `column`, as `EXPLAIN` shows them.
    fn describe(&self, column: &str) -> Vec<String> {
        let show = |op: &str, value: &Value| format!("{column} {op} {}", value.to_sql());
        if let Some(value) = &self.equal {
            return vec![show("=", value)];
        }
        let mut shown = Vec::new();
        if let Some((value, included)) = &self.lower {
            shown.push(show(if *included { ">=" } else { ">" }, value));
        }
        if let Some((value, included)) = &self.upper {
            shown.push(show(if *included { "<=" } else { "<" }, value));
        }
        shown
    }
}

/// Keep whichever bound rules out more. `wanted` is how a tighter value
/// compares with a looser one; on a tie, the excluded bound is tighter.
fn tighten(bound: &mut Option<(Value, bool)>, candidate: (Value, bool), wanted: Ordering) {
    let tighter = match bound {
        None => true,
        Some((value, included)) => {
            let ordering = candidate.0.sort_cmp(value);
            ordering == wanted || (ordering == Ordering::Equal && *included && !candidate.1)
        }
    };
    if tighter {
        *bound = Some(candidate);
    }
}

fn is_constant(expr: &Expr) -> bool {
    match expr {
        Expr::Literal(_) => true,
        Expr::Column(_) => false,
        Expr::Unary { expr, .. } | Expr::IsNull { expr, .. } => is_constant(expr),
        Expr::Binary { left, right, .. } => is_constant(left) && is_constant(right),
    }
}

/// `a < b` read from the other side: `b > a`.
fn flipped(op: BinaryOp) -> BinaryOp {
    match op {
        BinaryOp::Lt => BinaryOp::Gt,
        BinaryOp::LtEq => BinaryOp::GtEq,
        BinaryOp::Gt => BinaryOp::Lt,
        BinaryOp::GtEq => BinaryOp::LtEq,
        other => other,
    }
}

/// `value` as it would be stored in a column of type `ty`, if comparing them
/// in a key gives exactly the answer a full scan would. NULL matches nothing,
/// and INT against REAL compares with rounding, so neither qualifies. Other
/// mixes of types are errors, and a scan is what reports them.
fn exact_key_value(value: Value, ty: DataType) -> Option<Value> {
    match (value, ty) {
        (Value::Int(n), DataType::Int) => Some(Value::Int(n)),
        (Value::Real(r), DataType::Real) if !r.is_nan() => Some(Value::Real(r)),
        // This is the same conversion the comparison makes.
        (Value::Int(n), DataType::Real) => Some(Value::Real(n as f64)),
        (Value::Text(s), DataType::Text) => Some(Value::Text(s)),
        (Value::Bool(b), DataType::Bool) => Some(Value::Bool(b)),
        _ => None,
    }
}

/// Read what `expr`, a WHERE or one side of its ANDs, says about each column.
fn collect(expr: &Expr, table: &TableSchema, constraints: &mut [Constraint]) {
    let Expr::Binary { left, op, right } = expr else {
        return;
    };
    let (column, op, constant) = match (op, &**left, &**right) {
        (BinaryOp::And, _, _) => {
            collect(left, table, constraints);
            collect(right, table, constraints);
            return;
        }
        (BinaryOp::Eq | BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq, _, _) => {
            match (&**left, &**right) {
                (Expr::Column(name), other) if is_constant(other) => (name, *op, other),
                (other, Expr::Column(name)) if is_constant(other) => (name, flipped(*op), other),
                _ => return,
            }
        }
        _ => return,
    };
    let Some(i) = table.column_index(column) else {
        return;
    };
    // A constant that fails is left alone: on an empty table a scan never
    // evaluates it, so failing here would be a new error.
    let Ok(value) = eval(constant, &Scope::empty()) else {
        return;
    };
    if let Some(value) = exact_key_value(value, table.columns[i].data_type) {
        constraints[i].add(op, value);
    }
}

/// Better paths compare greater: which kind, then how many columns are
/// pinned to one value, then whether a range narrows it further.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Score(u8, usize, bool);

const PRIMARY_KEY_EQUAL: u8 = 4;
const INDEX_EQUAL: u8 = 3;
const PRIMARY_KEY_RANGE: u8 = 2;
const INDEX_RANGE: u8 = 1;

fn primary_key_path(table: &TableSchema, constraints: &[Constraint]) -> Option<(Score, Access)> {
    let column = table.primary_key()?;
    let constraint = &constraints[column];
    let conditions = constraint.describe(&table.columns[column].name);
    let key = |value: &Value| match value {
        Value::Int(n) => encode_key(*n).to_vec(),
        _ => unreachable!("a primary key is INT, and constants were checked against it"),
    };
    if let Some(value) = &constraint.equal {
        let range = KeyRange {
            start: Bound::Included(key(value)),
            end: Bound::Included(key(value)),
        };
        return Some((
            Score(PRIMARY_KEY_EQUAL, 1, false),
            Access::PrimaryKey {
                range,
                lookup: true,
                conditions,
            },
        ));
    }
    if !constraint.has_range() {
        return None;
    }
    let bound = |side: &Option<(Value, bool)>| match side {
        Some((value, true)) => Bound::Included(key(value)),
        Some((value, false)) => Bound::Excluded(key(value)),
        None => Bound::Unbounded,
    };
    let range = KeyRange {
        start: bound(&constraint.lower),
        end: bound(&constraint.upper),
    };
    Some((
        Score(PRIMARY_KEY_RANGE, 0, true),
        Access::PrimaryKey {
            range,
            lookup: false,
            conditions,
        },
    ))
}

/// The keys starting with `prefix` whose next column meets `constraint`.
///
/// Every key has the row id after its columns, so a bound can't name an exact
/// key. Instead, `successor` gives the point just past every key that starts
/// with some bytes. That is why even `a > 5` starts at an included bound.
fn index_range(prefix: &[u8], constraint: &Constraint) -> KeyRange {
    let with = |value: &Value| {
        let mut key = prefix.to_vec();
        encode_value(value, &mut key);
        key
    };
    let past = |key: Vec<u8>| successor(&key).map_or(Bound::Unbounded, Bound::Excluded);
    let start = match &constraint.lower {
        Some((value, true)) => Bound::Included(with(value)),
        Some((value, false)) => {
            let key = with(value);
            successor(&key).map_or(Bound::Excluded(key), Bound::Included)
        }
        // NULLs sort first, and a range never matches them.
        None if constraint.upper.is_some() => {
            let mut key = prefix.to_vec();
            key.push(FIRST_VALUE);
            Bound::Included(key)
        }
        None => Bound::Included(prefix.to_vec()),
    };
    let end = match &constraint.upper {
        Some((value, true)) => past(with(value)),
        Some((value, false)) => Bound::Excluded(with(value)),
        None => past(prefix.to_vec()),
    };
    KeyRange { start, end }
}

fn index_path(
    table: &TableSchema,
    index: &IndexSchema,
    constraints: &[Constraint],
) -> Option<(Score, Access)> {
    let mut prefix = Vec::new();
    let mut conditions = Vec::new();
    let mut equal = 0;
    let mut last = Constraint::default();
    // Equal columns narrow the key from the left, until the first that isn't
    // pinned to one value. Only that one can add a range; later ones can't.
    for name in &index.columns {
        let constraint = &constraints[table.column_index(name)?];
        conditions.extend(constraint.describe(name));
        match &constraint.equal {
            Some(value) => {
                encode_value(value, &mut prefix);
                equal += 1;
            }
            None => {
                last = constraint.clone();
                break;
            }
        }
    }
    let has_range = last.has_range();
    if equal == 0 && !has_range {
        return None;
    }
    let kind = if equal > 0 { INDEX_EQUAL } else { INDEX_RANGE };
    Some((
        Score(kind, equal, has_range),
        Access::Index {
            index: index.clone(),
            range: index_range(&prefix, &last),
            conditions,
        },
    ))
}

/// Pick how to find the rows `filter` wants. Without statistics this goes by
/// the shape of the query alone. `indexes` must be sorted by name, so that a
/// tie goes to the first name.
pub fn choose(table: &TableSchema, indexes: &[IndexSchema], filter: Option<&Expr>) -> Access {
    let Some(filter) = filter else {
        return Access::FullScan;
    };
    let mut constraints = vec![Constraint::default(); table.columns.len()];
    collect(filter, table, &mut constraints);

    let mut best = primary_key_path(table, &constraints);
    for index in indexes {
        if let Some((score, access)) = index_path(table, index, &constraints)
            && best.as_ref().is_none_or(|(top, _)| score > *top)
        {
            best = Some((score, access));
        }
    }
    best.map_or(Access::FullScan, |(_, access)| access)
}

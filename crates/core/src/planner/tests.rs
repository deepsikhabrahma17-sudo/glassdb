use super::*;
use crate::sql::{ColumnDef, Statement, parse_one};
use crate::types::DataType;

/// `t (id INT PRIMARY KEY, a INT, b TEXT, r REAL, f BOOL)`.
fn table() -> TableSchema {
    let column = |name: &str, data_type, primary_key| ColumnDef {
        name: name.into(),
        data_type,
        primary_key,
    };
    TableSchema {
        name: "t".into(),
        root: 2,
        columns: vec![
            column("id", DataType::Int, true),
            column("a", DataType::Int, false),
            column("b", DataType::Text, false),
            column("r", DataType::Real, false),
            column("f", DataType::Bool, false),
        ],
    }
}

fn index(name: &str, columns: &[&str]) -> IndexSchema {
    IndexSchema {
        name: name.into(),
        table: "t".into(),
        root: 3,
        columns: columns.iter().map(|c| c.to_string()).collect(),
    }
}

/// The plan's one-line description for `WHERE filter`.
fn plan(filter: &str, indexes: &[IndexSchema]) -> String {
    let sql = format!("SELECT * FROM t WHERE {filter}");
    let Statement::Select(select) = parse_one(&sql).unwrap() else {
        unreachable!()
    };
    choose(&table(), indexes, select.filter.as_ref()).describe("t")
}

fn all() -> Vec<IndexSchema> {
    vec![
        index("by_a", &["a"]),
        index("by_a_b", &["a", "b"]),
        index("by_b", &["b"]),
        index("by_r", &["r"]),
    ]
}

const SCAN: &str = "FULL SCAN t";

#[test]
fn no_filter_is_a_full_scan() {
    assert_eq!(choose(&table(), &all(), None).describe("t"), SCAN);
}

#[test]
fn the_primary_key_needs_no_index() {
    assert_eq!(plan("id = 5", &[]), "PRIMARY KEY LOOKUP ON t (id = 5)");
    assert_eq!(plan("id > 5", &[]), "PRIMARY KEY RANGE ON t (id > 5)");
    assert_eq!(
        plan("id >= 1 AND id < 9", &[]),
        "PRIMARY KEY RANGE ON t (id >= 1, id < 9)"
    );
}

#[test]
fn an_equality_uses_an_index() {
    assert_eq!(plan("a = 5", &all()), "INDEX SCAN by_a ON t (a = 5)");
    assert_eq!(plan("5 = a", &all()), "INDEX SCAN by_a ON t (a = 5)");
    assert_eq!(plan("b = 'x'", &all()), "INDEX SCAN by_b ON t (b = 'x')");
}

#[test]
fn a_range_uses_an_index_and_flips_when_written_backwards() {
    assert_eq!(plan("a > 5", &all()), "INDEX SCAN by_a ON t (a > 5)");
    assert_eq!(plan("a <= 5", &all()), "INDEX SCAN by_a ON t (a <= 5)");
    assert_eq!(plan("5 < a", &all()), "INDEX SCAN by_a ON t (a > 5)");
    assert_eq!(plan("5 >= a", &all()), "INDEX SCAN by_a ON t (a <= 5)");
}

#[test]
fn two_bounds_become_one_range() {
    assert_eq!(
        plan("a > 1 AND a < 5", &all()),
        "INDEX SCAN by_a ON t (a > 1, a < 5)"
    );
    assert_eq!(
        plan("a > 1 AND a > 3 AND a < 9 AND a <= 5", &all()),
        "INDEX SCAN by_a ON t (a > 3, a <= 5)"
    );
    assert_eq!(
        plan("a >= 3 AND a > 3", &all()),
        "INDEX SCAN by_a ON t (a > 3)"
    );
}

#[test]
fn a_constant_expression_is_worked_out_first() {
    assert_eq!(plan("a = 2 + 3", &all()), "INDEX SCAN by_a ON t (a = 5)");
    assert_eq!(plan("a = -1", &all()), "INDEX SCAN by_a ON t (a = -1)");
}

#[test]
fn a_multi_column_index_uses_its_leading_columns() {
    let indexes = [index("ab", &["a", "b", "r"])];
    assert_eq!(
        plan("a = 1 AND b = 'x'", &indexes),
        "INDEX SCAN ab ON t (a = 1, b = 'x')"
    );
    assert_eq!(
        plan("a = 1 AND b > 'x'", &indexes),
        "INDEX SCAN ab ON t (a = 1, b > 'x')"
    );
    assert_eq!(
        plan("a = 1 AND b = 'x' AND r < 2.5", &indexes),
        "INDEX SCAN ab ON t (a = 1, b = 'x', r < 2.5)"
    );
    // Nothing can follow a range.
    assert_eq!(
        plan("a > 1 AND b = 'x'", &indexes),
        "INDEX SCAN ab ON t (a > 1)"
    );
    // A gap stops it too.
    assert_eq!(
        plan("a = 1 AND r = 2.5", &indexes),
        "INDEX SCAN ab ON t (a = 1)"
    );
}

#[test]
fn an_index_is_useless_without_its_first_column() {
    assert_eq!(plan("b = 'x'", &[index("ab", &["a", "b"])]), SCAN);
}

#[test]
fn the_best_path_wins() {
    let by_a = index("by_a", &["a"]);
    let wide = index("wide", &["a", "b"]);
    // The primary key beats everything for an equality...
    assert_eq!(
        plan("id = 1 AND a = 1", std::slice::from_ref(&by_a)),
        "PRIMARY KEY LOOKUP ON t (id = 1)"
    );
    // ...and an index equality beats a primary key range.
    assert_eq!(
        plan("id > 1 AND a = 1", std::slice::from_ref(&by_a)),
        "INDEX SCAN by_a ON t (a = 1)"
    );
    // A primary key range beats an index range.
    assert_eq!(
        plan("id > 1 AND a > 1", std::slice::from_ref(&by_a)),
        "PRIMARY KEY RANGE ON t (id > 1)"
    );
    // More equal columns win, then a range on top, and ties go by name.
    assert_eq!(
        plan("a = 1 AND b = 'x'", &[by_a.clone(), wide.clone()]),
        "INDEX SCAN wide ON t (a = 1, b = 'x')"
    );
    assert_eq!(
        plan("a = 1 AND b > 'x'", &[by_a.clone(), wide.clone()]),
        "INDEX SCAN wide ON t (a = 1, b > 'x')"
    );
    assert_eq!(
        plan("a = 1", &[index("first", &["a"]), index("second", &["a"])]),
        "INDEX SCAN first ON t (a = 1)"
    );
}

#[test]
fn conditions_a_key_cannot_answer_fall_back_to_a_scan() {
    for filter in [
        "a = 1 OR a = 2",
        "a <> 1",
        "NOT a = 1",
        "a IS NULL",
        "a IS NOT NULL",
        "a = id",
        "a + 1 = 2",
        "f",
    ] {
        assert_eq!(plan(filter, &all()), SCAN, "{filter}");
    }
}

#[test]
fn only_top_level_ands_count() {
    assert_eq!(plan("a = 1 OR (a = 2 AND a = 3)", &all()), SCAN);
    assert_eq!(
        plan("(a = 1 OR a = 2) AND b = 'x'", &all()),
        "INDEX SCAN by_b ON t (b = 'x')"
    );
}

#[test]
fn a_null_constant_is_not_used() {
    assert_eq!(plan("a = NULL", &all()), SCAN);
    assert_eq!(plan("a > NULL", &all()), SCAN);
    assert_eq!(plan("id = NULL", &all()), SCAN);
}

#[test]
fn a_constant_that_fails_is_left_to_the_scan() {
    // Evaluating it now would raise the error even on an empty table.
    assert_eq!(plan("a = 1 / 0", &all()), SCAN);
    assert_eq!(plan("b = 1 + 'x'", &all()), SCAN);
}

#[test]
fn mismatched_types_fall_back_so_the_scan_can_report_them() {
    assert_eq!(plan("a = 'x'", &all()), SCAN);
    assert_eq!(plan("b = 1", &all()), SCAN);
    assert_eq!(plan("id = 'x'", &all()), SCAN);
}

#[test]
fn an_int_column_compared_with_a_real_falls_back() {
    // INT and REAL compare with rounding, so no exact key bound exists.
    assert_eq!(plan("a > 1.5", &all()), SCAN);
    assert_eq!(plan("id = 1.0", &all()), SCAN);
}

#[test]
fn a_real_column_takes_an_int_constant() {
    assert_eq!(plan("r > 5", &all()), "INDEX SCAN by_r ON t (r > 5.0)");
}

#[test]
fn a_not_a_number_constant_is_not_used() {
    let infinity = "1e308 * 10.0";
    assert_eq!(
        plan(&format!("r < {infinity}"), &all()),
        "INDEX SCAN by_r ON t (r < inf)"
    );
    assert_eq!(plan(&format!("r < {infinity} - {infinity}"), &all()), SCAN);
}

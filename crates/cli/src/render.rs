//! Showing results: rows as an aligned table, everything else as a line.

use glassdb_core::{Output, Value};

pub fn render(output: &Output) -> String {
    match output {
        Output::Rows { columns, rows } => table(columns, rows),
        other => other.to_string(),
    }
}

/// ```text
///  id | name
/// ----+------
///   1 | ada
/// (1 row)
/// ```
/// Number columns are right-aligned, everything else left-aligned.
fn table(columns: &[String], rows: &[Vec<Value>]) -> String {
    let cells: Vec<Vec<String>> = rows
        .iter()
        .map(|row| row.iter().map(Value::to_string).collect())
        .collect();
    let widths: Vec<usize> = (0..columns.len())
        .map(|c| {
            let header = columns[c].chars().count();
            cells
                .iter()
                .map(|row| row[c].chars().count())
                .fold(header, usize::max)
        })
        .collect();
    let numeric: Vec<bool> = (0..columns.len())
        .map(|c| {
            !rows.is_empty()
                && rows
                    .iter()
                    .all(|row| matches!(row[c], Value::Int(_) | Value::Real(_) | Value::Null))
        })
        .collect();

    let line = |texts: &[String], align_numbers: bool| -> String {
        let parts: Vec<String> = texts
            .iter()
            .enumerate()
            .map(|(c, text)| {
                let w = widths[c];
                if align_numbers && numeric[c] {
                    format!(" {text:>w$} ")
                } else {
                    format!(" {text:<w$} ")
                }
            })
            .collect();
        parts.join("|").trim_end().to_string()
    };

    let mut out = vec![line(columns, false)];
    let rule: Vec<String> = widths.iter().map(|w| "-".repeat(w + 2)).collect();
    out.push(rule.join("+"));
    for row in &cells {
        out.push(line(row, true));
    }
    out.push(match rows.len() {
        1 => "(1 row)".to_string(),
        n => format!("({n} rows)"),
    });
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_become_an_aligned_table() {
        let output = Output::Rows {
            columns: vec!["id".into(), "name".into()],
            rows: vec![
                vec![Value::Int(1), Value::Text("ada".into())],
                vec![Value::Int(10), Value::Null],
            ],
        };
        assert_eq!(
            render(&output),
            " id | name\n----+------\n  1 | ada\n 10 | NULL\n(2 rows)"
        );
    }

    #[test]
    fn empty_result_and_messages() {
        let output = Output::Rows {
            columns: vec!["a".into()],
            rows: vec![],
        };
        assert_eq!(render(&output), " a\n---\n(0 rows)");
        assert_eq!(render(&Output::Inserted(2)), "inserted 2 rows");
    }
}

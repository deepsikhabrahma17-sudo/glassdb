//! Runs every `tests/sql/*.test` file against a real database file.
//!
//! File format (blank lines separate records, `#` starts a comment line):
//!
//! ```text
//! statement ok
//! CREATE TABLE t (a INT)
//!
//! statement error no such table
//! SELECT * FROM missing
//!
//! query
//! SELECT a FROM t
//! ----
//! a
//! 1
//!
//! restart
//! ```
//!
//! - `statement ok`: the SQL must succeed.
//! - `statement error <text>`: the SQL must fail with a message containing `<text>`.
//! - `query`: the SQL's output must equal the lines after `----` exactly.
//! - `restart`: close the database and reopen it from the file on disk.

use std::path::{Path, PathBuf};

use glassdb_core::Database;

/// Run one test file and return how many statements it ran.
fn run_file(path: &Path) -> usize {
    let text = std::fs::read_to_string(path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let mut db = Some(Database::open(&db_path).unwrap());
    let mut statements = 0;

    let mut i = 0;
    while i < lines.len() {
        let directive = lines[i].trim_end();
        let at = format!("{}:{}", path.display(), i + 1);
        i += 1;
        if directive.trim().is_empty() || directive.starts_with('#') {
            continue;
        }
        if directive == "restart" {
            db.take().unwrap().close().unwrap();
            db = Some(Database::open(&db_path).unwrap());
            continue;
        }

        // The SQL runs until a blank line, or until `----` for a query.
        let mut sql = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i] != "----" {
            sql.push(lines[i]);
            i += 1;
        }
        let sql = sql.join("\n");
        assert!(!sql.is_empty(), "{at}: `{directive}` has no SQL");
        let result = db.as_mut().unwrap().execute(&sql);
        statements += 1;

        if directive == "statement ok" {
            if let Err(err) = result {
                panic!("{at}: expected success, got error: {err}\n    {sql}");
            }
        } else if let Some(want) = directive.strip_prefix("statement error") {
            let want = want.trim();
            match result {
                Ok(out) => {
                    panic!("{at}: expected an error containing {want:?}, got:\n{out}\n    {sql}")
                }
                Err(err) => assert!(
                    err.to_string().contains(want),
                    "{at}: expected an error containing {want:?}, got: {err}\n    {sql}"
                ),
            }
        } else if directive == "query" {
            assert_eq!(
                lines.get(i),
                Some(&"----"),
                "{at}: a query needs a `----` line"
            );
            i += 1;
            let mut expected = Vec::new();
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i]);
                i += 1;
            }
            let got = match result {
                Ok(out) => out.to_string(),
                Err(err) => panic!("{at}: query failed: {err}\n    {sql}"),
            };
            assert_eq!(
                got,
                expected.join("\n"),
                "{at}: wrong output for\n    {sql}"
            );
        } else {
            panic!("{at}: unknown directive {directive:?}");
        }
    }
    db.take().unwrap().close().unwrap();
    statements
}

#[test]
fn sql_test_files() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/sql");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "test"))
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no .test files in {}", dir.display());

    for file in files {
        let count = run_file(&file);
        let name = file.file_name().unwrap().to_string_lossy().into_owned();
        println!("{name}: {count} statements passed");
        if name == "basics.test" {
            assert!(
                count >= 50,
                "basics.test should run 50+ statements, ran {count}"
            );
        }
    }
}

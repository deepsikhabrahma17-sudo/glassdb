//! End-to-end tests: run the real `glassdb` binary with piped stdin.

use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Run `glassdb` with `args`, feed it `input` on stdin, and wait for it to exit.
fn run(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassdb"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn glassdb");
    // glassdb may exit before reading its input (for example when it refuses
    // to open the file). Its end of the pipe is then closed, so the write
    // fails with BrokenPipe. That is expected, not a test failure.
    let written = child.stdin.take().unwrap().write_all(input.as_bytes());
    if let Err(err) = written {
        assert_eq!(err.kind(), ErrorKind::BrokenPipe, "writing stdin: {err}");
    }
    child.wait_with_output().expect("wait for glassdb")
}

/// Run `glassdb <db>` and return (stdout, stderr), checking it exited cleanly.
fn session(db: &Path, input: &str) -> (String, String) {
    let out = run(&[db.to_str().unwrap()], input);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(
        out.status.success(),
        "exit {:?}\n{stdout}\n{stderr}",
        out.status
    );
    (stdout, stderr)
}

#[test]
fn runs_sql_and_prints_a_table() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, stderr) = session(
        &dir.path().join("test.db"),
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT);\n\
         INSERT INTO t VALUES (1, 'ada'), (10, NULL);\n\
         SELECT * FROM t ORDER BY id;\n",
    );
    assert!(stderr.is_empty(), "{stderr}");
    assert!(
        stdout.contains("test.db"),
        "banner names the file: {stdout}"
    );
    assert!(stdout.contains("created table t"), "{stdout}");
    assert!(stdout.contains("inserted 2 rows"), "{stdout}");
    assert!(
        stdout.contains(" id | name\n----+------\n  1 | ada\n 10 | NULL\n(2 rows)"),
        "{stdout}"
    );
}

#[test]
fn statements_can_span_lines_and_share_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, _) = session(
        &dir.path().join("test.db"),
        "SELECT\n  1 + 2\n  AS three;\nSELECT 'a;b' AS s; SELECT 4 AS four;\n",
    );
    assert!(
        stdout.contains(" three\n-------\n     3\n(1 row)"),
        "{stdout}"
    );
    assert!(
        stdout.contains("a;b"),
        "a ';' inside a string doesn't end the statement: {stdout}"
    );
    assert!(stdout.contains(" four\n"), "{stdout}");
}

#[test]
fn errors_are_reported_and_the_session_continues() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, stderr) = session(
        &dir.path().join("test.db"),
        "SELECT * FROM nope;\nSELECT 41 + 1 AS n;\n",
    );
    assert!(stderr.contains("error: no such table: nope"), "{stderr}");
    assert!(stdout.contains("42"), "{stdout}");
}

#[test]
fn data_survives_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("keep.db");
    session(
        &db,
        "CREATE TABLE t (a TEXT);\nINSERT INTO t VALUES ('kept');\n",
    );
    let (stdout, _) = session(&db, "SELECT a FROM t;\n");
    assert!(
        stdout.contains("(1 table)"),
        "banner counts tables: {stdout}"
    );
    assert!(stdout.contains("kept"), "{stdout}");
}

#[test]
fn dot_commands() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, stderr) = session(
        &dir.path().join("test.db"),
        "CREATE TABLE b (x INT);\nCREATE TABLE a (id INT PRIMARY KEY);\n.tables\n.schema a\n.help\n.bogus\n.exit\nSELECT 'not reached';\n",
    );
    assert!(
        stdout.contains("a\nb\n"),
        ".tables lists names in order: {stdout}"
    );
    assert!(
        stdout.contains("CREATE TABLE a (id INT PRIMARY KEY);"),
        "{stdout}"
    );
    assert!(stdout.contains(".schema"), ".help lists commands: {stdout}");
    assert!(stderr.contains("unknown command .bogus"), "{stderr}");
    assert!(
        !stdout.contains("not reached"),
        ".exit stops reading: {stdout}"
    );
}

#[test]
fn check_and_checkpoint_commands() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, stderr) = session(
        &dir.path().join("test.db"),
        "CREATE TABLE t (a INT);\nINSERT INTO t VALUES (1);\n.check\n.checkpoint\n",
    );
    assert!(stderr.is_empty(), "{stderr}");
    assert!(stdout.contains("ok: 1 table, 1 row"), "{stdout}");
    assert!(stdout.contains("checkpoint done"), "{stdout}");
}

#[test]
fn the_banner_mentions_recovered_work() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("test.db");
    let mut db = glassdb_core::Database::open(&db_path).unwrap();
    db.execute("CREATE TABLE t (a INT)").unwrap();
    drop(db); // a crash: never closed

    let (stdout, _) = session(&db_path, "SELECT a FROM t;\n");
    assert!(stdout.contains("recovered 2 transactions"), "{stdout}");
    assert!(
        !dir.path().join("test.db-wal").exists(),
        "a clean exit removes the log"
    );
}

#[test]
fn end_of_input_runs_an_unfinished_statement_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let (stdout, _) = session(&dir.path().join("test.db"), "SELECT 7 AS last");
    assert!(stdout.contains(" last\n"), "{stdout}");
}

#[test]
fn missing_path_prints_usage() {
    let out = run(&[], "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr.contains("usage"), "stderr: {stderr}");
}

#[test]
fn creates_a_new_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("new.db");
    let (stdout, _) = session(&db, ".exit\n");
    let bytes = std::fs::read(&db).expect("database file was created");
    assert_eq!(bytes.len(), 2 * 4096, "header page + catalog page");
    assert_eq!(&bytes[..8], b"GLASSDB\0");
    assert!(stdout.contains("(0 tables)"), "{stdout}");
}

#[test]
fn rejects_a_file_that_is_not_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("notes.txt");
    std::fs::write(&db, vec![0xFF; 4096]).unwrap();

    let out = run(&[db.to_str().unwrap()], ".exit\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr.contains("not a GlassDB database"),
        "stderr: {stderr}"
    );
    // The file is left untouched.
    assert_eq!(std::fs::read(&db).unwrap(), vec![0xFF; 4096]);
}

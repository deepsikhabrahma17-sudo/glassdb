//! End-to-end tests: run the real `glassdb` binary with piped stdin.

use std::io::Write;
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
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    child.wait_with_output().expect("wait for glassdb")
}

/// Run `glassdb <db>` on a database file inside a temporary directory.
fn run_db(db: &Path, input: &str) -> Output {
    run(&[db.to_str().unwrap()], input)
}

#[test]
fn opens_prompt_and_echoes() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_db(&dir.path().join("test.db"), "hello\n.exit\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(stdout.contains("GlassDB"), "no banner in: {stdout}");
    assert!(
        stdout.contains("test.db"),
        "banner should name the file: {stdout}"
    );
    assert!(stdout.contains("hello"), "no echo in: {stdout}");
}

#[test]
fn end_of_input_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let out = run_db(&dir.path().join("test.db"), "one\ntwo\n");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "exit status: {:?}", out.status);
    assert!(stdout.contains("one") && stdout.contains("two"), "{stdout}");
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
    let out = run_db(&db, ".exit\n");
    assert!(out.status.success(), "exit status: {:?}", out.status);

    let bytes = std::fs::read(&db).expect("database file was created");
    assert_eq!(bytes.len(), 4096, "a new database is one header page");
    assert_eq!(&bytes[..8], b"GLASSDB\0");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("1 page"),
        "banner shows page count: {stdout}"
    );
}

#[test]
fn rejects_a_file_that_is_not_a_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("notes.txt");
    std::fs::write(&db, vec![0xFF; 4096]).unwrap();

    let out = run_db(&db, ".exit\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr.contains("not a GlassDB database"),
        "stderr: {stderr}"
    );
    // The file is left untouched.
    assert_eq!(std::fs::read(&db).unwrap(), vec![0xFF; 4096]);
}

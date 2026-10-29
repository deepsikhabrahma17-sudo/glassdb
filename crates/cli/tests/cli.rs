//! End-to-end tests: run the real `glassdb` binary with piped stdin.

use std::io::Write;
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

#[test]
fn opens_prompt_and_echoes() {
    let out = run(&["test.db"], "hello\n.exit\n");
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
    let out = run(&["test.db"], "one\ntwo\n");
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

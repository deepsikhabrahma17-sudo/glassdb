//! `glassdb <file>`: an interactive SQL prompt for a GlassDB database file.

mod render;
mod repl;

use std::process::ExitCode;

use glassdb_core::{Database, VERSION, sql};
use render::render;
use repl::{Action, Repl};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

const HELP: &str = "\
Enter SQL statements ending with ';'. Statements can span several lines.

Commands:
  .tables          list the tables
  .schema [name]   show CREATE TABLE statements
  .check           verify the whole database
  .checkpoint      move the log's pages into the main file
  .help            show this help
  .exit            leave (Ctrl-D works too)";

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: glassdb <database-file>");
        return ExitCode::from(2);
    };

    // Opens the file, or creates a new, empty database if it doesn't exist.
    let mut db = match Database::open(&path) {
        Ok(db) => db,
        Err(err) => {
            eprintln!("error: cannot open {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let tables = db.tables().map(|t| t.len()).unwrap_or(0);
    let unit = if tables == 1 { "table" } else { "tables" };
    println!("GlassDB {VERSION} — {path} ({tables} {unit})");
    match db.recovered() {
        0 => {}
        1 => println!("recovered 1 transaction from the log"),
        n => println!("recovered {n} transactions from the log"),
    }
    println!("Enter SQL ending with \";\", or .help for commands.");

    // rustyline gives line editing and history on a terminal, and falls back
    // to plain line reads when stdin is a pipe (which the tests rely on).
    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("error: cannot start prompt: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut repl = Repl::default();

    loop {
        match editor.readline(repl.prompt()) {
            Ok(line) => {
                // History is a convenience; failing to record it is harmless.
                let _ = editor.add_history_entry(line.as_str());
                match repl.feed(&line) {
                    Action::Run(sql) => run_sql(&mut db, &sql),
                    Action::Command(command) => run_command(&mut db, &command),
                    Action::Exit => break,
                    Action::More | Action::Skip => {}
                }
            }
            // Ctrl-C drops the statement being typed, like most shells.
            Err(ReadlineError::Interrupted) => repl.clear(),
            // Ctrl-D or end of piped input: run anything left unfinished.
            Err(ReadlineError::Eof) => {
                if let Some(sql) = repl.finish() {
                    run_sql(&mut db, &sql);
                }
                break;
            }
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Err(err) = db.close() {
        eprintln!("error: cannot save {path}: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Run every statement in `text`, stopping at the first error.
fn run_sql(db: &mut Database, text: &str) {
    let statements = match sql::parse(text) {
        Ok(statements) => statements,
        Err(err) => {
            eprintln!("error: {err}");
            return;
        }
    };
    for statement in &statements {
        match db.run(statement) {
            Ok(output) => println!("{}", render(&output)),
            Err(err) => {
                eprintln!("error: {err}");
                return;
            }
        }
    }
}

fn run_command(db: &mut Database, command: &str) {
    let mut words = command.split_whitespace();
    let result = match words.next().unwrap_or_default() {
        ".tables" => db.tables().map(|tables| {
            for table in tables {
                println!("{}", table.name);
            }
        }),
        ".schema" => {
            let only = words.next().map(str::to_lowercase);
            db.tables().map(|tables| {
                let shown: Vec<_> = tables
                    .iter()
                    .filter(|t| only.as_ref().is_none_or(|name| &t.name == name))
                    .collect();
                if shown.is_empty()
                    && let Some(name) = &only
                {
                    eprintln!("error: no such table: {name}");
                }
                for table in shown {
                    println!("{};", table.create_sql());
                }
            })
        }
        ".check" => db.check().map(|report| {
            println!(
                "ok: {}, {}, {} ({} free)",
                count(report.tables, "table"),
                count(report.rows, "row"),
                count(report.pages as usize, "page"),
                report.free_pages
            );
        }),
        ".checkpoint" => db.checkpoint().map(|()| println!("checkpoint done")),
        ".help" => {
            println!("{HELP}");
            Ok(())
        }
        other => {
            eprintln!("error: unknown command {other} (try .help)");
            Ok(())
        }
    };
    if let Err(err) = result {
        eprintln!("error: {err}");
    }
}

fn count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

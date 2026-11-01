//! `glassdb <file>`: an interactive SQL prompt for a GlassDB database file.

mod render;
mod repl;

use std::process::ExitCode;

use glassdb_core::sql::{Statement, When};
use glassdb_core::time::format_timestamp;
use glassdb_core::{Database, Reader, Result, VERSION, sql};
use render::render;
use repl::{Action, Repl};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

const HELP: &str = "\
Enter SQL statements ending with ';'. Statements can span several lines.
BEGIN starts a transaction, shown by a * in the prompt. COMMIT keeps it and
ROLLBACK undoes it. One still open when you leave is rolled back.
EXPLAIN before a SELECT, UPDATE or DELETE shows how it would run.

Every change is kept. After FROM <table>, add AS OF '<time>' or AS OF TXN <n> to
see the past, or HISTORY to see every version. Times are UTC unless you add an
offset, such as '2026-11-01 10:00 +05:30'. VACUUM HISTORY BEFORE '<time>' frees
what only earlier moments could see.

Commands:
  .tables          list the tables
  .schema [name]   show CREATE TABLE and CREATE INDEX statements
  .commits [n]     list the last n transactions and their times (default 10)
  .snapshot        make SELECTs keep reading this moment while you write
  .snapshot off    go back to reading the present
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
    let mut session = Session { db, snapshot: None };

    loop {
        let prompt = repl.prompt(
            session.db.in_transaction(),
            session.snapshot.as_ref().map(Reader::txn),
        );
        match editor.readline(&prompt) {
            Ok(line) => {
                // History is a convenience; failing to record it is harmless.
                let _ = editor.add_history_entry(line.as_str());
                match repl.feed(&line) {
                    Action::Run(sql) => session.run_sql(&sql),
                    Action::Command(command) => session.run_command(&command),
                    Action::Exit => break,
                    Action::More | Action::Skip => {}
                }
            }
            // Ctrl-C drops the statement being typed, like most shells.
            Err(ReadlineError::Interrupted) => repl.clear(),
            // Ctrl-D or end of piped input: run anything left unfinished.
            Err(ReadlineError::Eof) => {
                if let Some(sql) = repl.finish() {
                    session.run_sql(&sql);
                }
                break;
            }
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if session.db.in_transaction() {
        println!("open transaction rolled back");
    }
    if let Err(err) = session.db.close() {
        eprintln!("error: cannot save {path}: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// The open database, and the snapshot SELECTs are reading, if there is one.
struct Session {
    db: Database,
    snapshot: Option<Reader>,
}

impl Session {
    /// Run every statement in `text`, stopping at the first error.
    fn run_sql(&mut self, text: &str) {
        let statements = match sql::parse(text) {
            Ok(statements) => statements,
            Err(err) => {
                eprintln!("error: {err}");
                return;
            }
        };
        for statement in &statements {
            let result = match &self.snapshot {
                Some(reader) if reads_the_present(statement) => reader.run(statement),
                _ => self.db.run(statement),
            };
            match result {
                Ok(output) => println!("{}", render(&output)),
                Err(err) => {
                    eprintln!("error: {err}");
                    return;
                }
            }
        }
    }

    fn snapshot(&mut self, argument: Option<&str>) -> Result<()> {
        match argument {
            None => {
                let reader = self.db.reader()?;
                println!(
                    "SELECTs now read txn {}. .snapshot off goes back to the present.",
                    reader.txn()
                );
                self.snapshot = Some(reader);
            }
            Some("off") => {
                self.snapshot = None;
                println!("SELECTs read the present");
            }
            Some(other) => eprintln!("error: .snapshot takes nothing or off, not {other}"),
        }
        Ok(())
    }

    fn commits(&mut self, argument: Option<&str>) -> Result<()> {
        let limit = match argument.map(str::parse) {
            None => 10,
            Some(Ok(limit)) => limit,
            Some(Err(_)) => {
                eprintln!("error: .commits takes a number");
                return Ok(());
            }
        };
        let commits = self.db.commits(limit)?;
        if commits.is_empty() {
            println!("no commits yet");
        }
        for commit in commits {
            println!("txn {}  {}", commit.txn, format_timestamp(commit.time));
        }
        Ok(())
    }

    fn run_command(&mut self, command: &str) {
        let db = &mut self.db;
        let mut words = command.split_whitespace();
        let result = match words.next().unwrap_or_default() {
            ".tables" => db.tables().map(|tables| {
                for table in tables {
                    println!("{}", table.name);
                }
            }),
            ".schema" => {
                let only = words.next().map(str::to_lowercase);
                db.tables().and_then(|tables| {
                    let indexes = db.indexes()?;
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
                        for index in indexes.iter().filter(|i| i.table == table.name) {
                            println!("{};", index.create_sql());
                        }
                    }
                    Ok(())
                })
            }
            ".check" => db.check().map(|report| {
                println!(
                    "ok: {}, {}, {}, {}, {} ({} free)",
                    count(report.tables, "table"),
                    count(report.rows, "row"),
                    count(report.old_versions, "old version"),
                    count(report.dropped_tables, "dropped table"),
                    count(report.pages as usize, "page"),
                    report.free_pages
                );
            }),
            ".checkpoint" => db.checkpoint().map(|()| println!("checkpoint done")),
            ".commits" => {
                let argument = words.next();
                self.commits(argument)
            }
            ".snapshot" => {
                let argument = words.next();
                self.snapshot(argument)
            }
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
}

/// A SELECT of the current rows, which a snapshot answers instead. A SELECT
/// that names its own moment, and anything that writes, goes to the database.
fn reads_the_present(statement: &Statement) -> bool {
    match statement {
        Statement::Select(select) => select.when == When::Now,
        Statement::Explain(inner) => reads_the_present(inner),
        _ => false,
    }
}

fn count(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

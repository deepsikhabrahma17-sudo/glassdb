//! `glassdb <file>`: an interactive prompt for a GlassDB database file.

mod repl;

use std::process::ExitCode;

use glassdb_core::{Pager, VERSION};
use repl::{Action, handle_line};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: glassdb <database-file>");
        return ExitCode::from(2);
    };

    // Opens the file, or creates a new one-page database if it doesn't exist.
    let pager = match Pager::open_file(&path) {
        Ok(pager) => pager,
        Err(err) => {
            eprintln!("error: cannot open {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    let pages = pager.page_count();
    let unit = if pages == 1 { "page" } else { "pages" };
    println!("GlassDB {VERSION} — {path} ({pages} {unit})");
    println!("Enter \".exit\" to quit.");

    // rustyline gives line editing and history on a terminal, and falls back
    // to plain line reads when stdin is a pipe (which the tests rely on).
    let mut editor = match DefaultEditor::new() {
        Ok(editor) => editor,
        Err(err) => {
            eprintln!("error: cannot start prompt: {err}");
            return ExitCode::FAILURE;
        }
    };

    loop {
        match editor.readline("glassdb> ") {
            Ok(line) => {
                // History is a convenience; failing to record it is harmless.
                let _ = editor.add_history_entry(line.as_str());
                match handle_line(&line) {
                    Action::Echo(text) => println!("{text}"),
                    Action::Skip => {}
                    Action::Exit => break,
                }
            }
            // Ctrl-C clears the current line, like most shells.
            Err(ReadlineError::Interrupted) => continue,
            // Ctrl-D or end of piped input.
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Err(err) = pager.close() {
        eprintln!("error: cannot save {path}: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

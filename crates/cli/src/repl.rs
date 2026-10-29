//! Prompt logic, kept free of terminal I/O so it can be unit tested.

/// What the prompt should do with one line of input.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Print this text back. (Phase 0 only echoes; later phases run SQL.)
    Echo(String),
    /// Blank line: do nothing and prompt again.
    Skip,
    /// `.exit` or `.quit`: leave the prompt.
    Exit,
}

/// Decide what to do with one line typed at the prompt.
pub fn handle_line(line: &str) -> Action {
    match line.trim() {
        "" => Action::Skip,
        ".exit" | ".quit" => Action::Exit,
        text => Action::Echo(text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn echoes_text() {
        assert_eq!(handle_line("hello"), Action::Echo("hello".into()));
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(
            handle_line("  select 1;\t\n"),
            Action::Echo("select 1;".into())
        );
    }

    #[test]
    fn blank_lines_are_skipped() {
        assert_eq!(handle_line(""), Action::Skip);
        assert_eq!(handle_line("   \t"), Action::Skip);
    }

    #[test]
    fn exit_commands() {
        assert_eq!(handle_line(".exit"), Action::Exit);
        assert_eq!(handle_line(".quit"), Action::Exit);
        assert_eq!(handle_line("  .exit  "), Action::Exit);
    }
}

//! Prompt logic, kept free of terminal I/O so it can be unit tested.
//!
//! Lines are collected until the text ends with a `;` (one that isn't inside
//! a string or a comment), so a statement can span several lines.

/// What the prompt should do after a line of input.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// A complete statement (or several) to run.
    Run(String),
    /// A dot command such as `.tables`.
    Command(String),
    /// `.exit` or `.quit`.
    Exit,
    /// The statement isn't finished: read another line.
    More,
    /// A blank line between statements: nothing to do.
    Skip,
}

#[derive(Default)]
pub struct Repl {
    /// Lines of the statement typed so far.
    buffer: String,
}

impl Repl {
    pub fn prompt(&self) -> &'static str {
        if self.buffer.is_empty() {
            "glassdb> "
        } else {
            "   ...> "
        }
    }

    pub fn feed(&mut self, line: &str) -> Action {
        let trimmed = line.trim();
        if self.buffer.is_empty() {
            if trimmed.is_empty() {
                return Action::Skip;
            }
            // Dot commands are only recognised at the start of a statement.
            if trimmed.starts_with('.') {
                return match trimmed {
                    ".exit" | ".quit" => Action::Exit,
                    _ => Action::Command(trimmed.to_string()),
                };
            }
        }
        self.buffer.push_str(line);
        self.buffer.push('\n');
        if ends_statement(&self.buffer) {
            Action::Run(std::mem::take(&mut self.buffer).trim().to_string())
        } else {
            Action::More
        }
    }

    /// At end of input: whatever was typed but never finished with `;`.
    pub fn finish(&mut self) -> Option<String> {
        let sql = std::mem::take(&mut self.buffer).trim().to_string();
        (!sql.is_empty()).then_some(sql)
    }

    /// Forget a half-typed statement (Ctrl-C).
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

/// Whether `text` ends with a `;` outside any 'string' and `--` comment.
fn ends_statement(text: &str) -> bool {
    let mut in_string = false;
    let mut last = None; // last character that counts, outside strings and comments
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            // `''` inside a string closes and reopens it, which works out the same.
            in_string = c != '\'';
            continue;
        }
        match c {
            '\'' => {
                in_string = true;
                last = Some(c);
            }
            '-' if chars.peek() == Some(&'-') => while chars.next_if(|&n| n != '\n').is_some() {},
            c if c.is_whitespace() => {}
            c => last = Some(c),
        }
    }
    !in_string && last == Some(';')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_all(lines: &[&str]) -> Vec<Action> {
        let mut repl = Repl::default();
        lines.iter().map(|line| repl.feed(line)).collect()
    }

    #[test]
    fn one_line_statement() {
        assert_eq!(
            feed_all(&["SELECT 1;"]),
            vec![Action::Run("SELECT 1;".into())]
        );
    }

    #[test]
    fn statement_over_several_lines() {
        assert_eq!(
            feed_all(&["SELECT", "  1", "  ;"]),
            vec![
                Action::More,
                Action::More,
                Action::Run("SELECT\n  1\n  ;".into())
            ]
        );
    }

    #[test]
    fn semicolons_in_strings_and_comments_dont_count() {
        assert_eq!(feed_all(&["SELECT 'a;b'"]), vec![Action::More]);
        assert_eq!(feed_all(&["SELECT 'it''s;'"]), vec![Action::More]);
        assert_eq!(feed_all(&["SELECT 1 -- done;"]), vec![Action::More]);
        assert_eq!(
            feed_all(&["SELECT 1; -- done"])[0],
            Action::Run("SELECT 1; -- done".into())
        );
    }

    #[test]
    fn commands_blank_lines_and_exit() {
        assert_eq!(
            feed_all(&["", "  .tables  ", ".exit", ".quit"]),
            vec![
                Action::Skip,
                Action::Command(".tables".into()),
                Action::Exit,
                Action::Exit
            ]
        );
    }

    #[test]
    fn prompt_shows_an_unfinished_statement() {
        let mut repl = Repl::default();
        assert_eq!(repl.prompt(), "glassdb> ");
        repl.feed("SELECT");
        assert_eq!(repl.prompt(), "   ...> ");
        assert_eq!(repl.finish(), Some("SELECT".into()));
        assert_eq!(repl.prompt(), "glassdb> ");
        assert_eq!(repl.finish(), None);
    }
}

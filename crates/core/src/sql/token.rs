//! The tokenizer: turns SQL text into a list of tokens, each tagged with the
//! line and column where it starts, so errors can point at the right place.

use std::fmt;

use crate::error::{Error, Result};

/// Words with a fixed meaning in SQL. Matched in any case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Keyword {
    And,
    As,
    Asc,
    By,
    Create,
    Delete,
    Desc,
    Drop,
    Exists,
    False,
    From,
    If,
    Insert,
    Into,
    Is,
    Key,
    Limit,
    Not,
    Null,
    Offset,
    Or,
    Order,
    Primary,
    Select,
    Set,
    Table,
    True,
    Update,
    Values,
    Where,
}

impl Keyword {
    fn from_word(word: &str) -> Option<Keyword> {
        use Keyword::*;
        Some(match word {
            "and" => And,
            "as" => As,
            "asc" => Asc,
            "by" => By,
            "create" => Create,
            "delete" => Delete,
            "desc" => Desc,
            "drop" => Drop,
            "exists" => Exists,
            "false" => False,
            "from" => From,
            "if" => If,
            "insert" => Insert,
            "into" => Into,
            "is" => Is,
            "key" => Key,
            "limit" => Limit,
            "not" => Not,
            "null" => Null,
            "offset" => Offset,
            "or" => Or,
            "order" => Order,
            "primary" => Primary,
            "select" => Select,
            "set" => Set,
            "table" => Table,
            "true" => True,
            "update" => Update,
            "values" => Values,
            "where" => Where,
            _ => return None,
        })
    }

    /// The keyword in upper case, as printed in SQL and error messages.
    pub fn as_str(self) -> String {
        format!("{self:?}").to_uppercase()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Keyword(Keyword),
    /// A table or column name, lowercased (names are case-insensitive).
    Ident(String),
    /// A number exactly as written; the parser decides INT or REAL.
    Number(String),
    /// A '…' string literal, with `''` already turned into `'`.
    Text(String),
    LParen,
    RParen,
    Comma,
    Semicolon,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Eof,
}

impl fmt::Display for Token {
    /// How a token is named in error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let symbol = match self {
            Token::Keyword(k) => return write!(f, "{}", k.as_str()),
            Token::Ident(name) => return write!(f, "{name}"),
            Token::Number(n) => return write!(f, "{n}"),
            Token::Text(s) => return write!(f, "'{}'", s.replace('\'', "''")),
            Token::Eof => return write!(f, "end of input"),
            Token::LParen => "(",
            Token::RParen => ")",
            Token::Comma => ",",
            Token::Semicolon => ";",
            Token::Star => "*",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Slash => "/",
            Token::Percent => "%",
            Token::Eq => "=",
            Token::NotEq => "<>",
            Token::Lt => "<",
            Token::LtEq => "<=",
            Token::Gt => ">",
            Token::GtEq => ">=",
        };
        write!(f, "'{symbol}'")
    }
}

/// A token and where it starts (1-based line and column).
#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub line: usize,
    pub column: usize,
}

/// Split `sql` into tokens. The list always ends with `Token::Eof`.
pub fn tokenize(sql: &str) -> Result<Vec<Spanned>> {
    let mut lexer = Lexer {
        chars: sql.chars().collect(),
        pos: 0,
        line: 1,
        column: 1,
    };
    let mut tokens = Vec::new();
    loop {
        lexer.skip_space_and_comments();
        let (line, column) = (lexer.line, lexer.column);
        let token = match lexer.peek() {
            None => Token::Eof,
            Some(_) => lexer.token(line, column)?,
        };
        let done = token == Token::Eof;
        tokens.push(Spanned {
            token,
            line,
            column,
        });
        if done {
            return Ok(tokens);
        }
    }
}

struct Lexer {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    column: usize,
}

impl Lexer {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_second(&self) -> Option<char> {
        self.chars.get(self.pos + 1).copied()
    }

    /// Move past one character, keeping the line and column up to date.
    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
            self.column = 1;
        } else {
            self.column += 1;
        }
        Some(c)
    }

    fn skip_space_and_comments(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => {
                    self.bump();
                }
                // `--` starts a comment that runs to the end of the line.
                Some('-') if self.peek_second() == Some('-') => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.bump();
                    }
                }
                _ => return,
            }
        }
    }

    /// Read one token starting at the current character.
    fn token(&mut self, line: usize, column: usize) -> Result<Token> {
        let error = |message: String| Error::Syntax {
            message,
            line,
            column,
        };
        let c = self.bump().expect("caller checked there is a character");
        let token = match c {
            c if c.is_alphabetic() || c == '_' => {
                let mut word = c.to_lowercase().to_string();
                while let Some(next) = self.peek().filter(|n| n.is_alphanumeric() || *n == '_') {
                    word.extend(next.to_lowercase());
                    self.bump();
                }
                Keyword::from_word(&word).map_or(Token::Ident(word), Token::Keyword)
            }
            c if c.is_ascii_digit() => Token::Number(self.number(c)),
            '\'' => {
                let mut text = String::new();
                loop {
                    match self.bump() {
                        None => {
                            return Err(error("unterminated string (missing closing ')".into()));
                        }
                        // Two quotes in a row stand for one quote character.
                        Some('\'') if self.peek() == Some('\'') => {
                            self.bump();
                            text.push('\'');
                        }
                        Some('\'') => break,
                        Some(ch) => text.push(ch),
                    }
                }
                Token::Text(text)
            }
            '"' => {
                return Err(error(
                    "use single quotes for strings ('text'); double quotes are not supported"
                        .into(),
                ));
            }
            '(' => Token::LParen,
            ')' => Token::RParen,
            ',' => Token::Comma,
            ';' => Token::Semicolon,
            '*' => Token::Star,
            '+' => Token::Plus,
            '-' => Token::Minus,
            '/' => Token::Slash,
            '%' => Token::Percent,
            '=' => {
                if self.peek() == Some('=') {
                    self.bump();
                }
                Token::Eq
            }
            '!' if self.peek() == Some('=') => {
                self.bump();
                Token::NotEq
            }
            '<' => match self.peek() {
                Some('=') => {
                    self.bump();
                    Token::LtEq
                }
                Some('>') => {
                    self.bump();
                    Token::NotEq
                }
                _ => Token::Lt,
            },
            '>' => {
                if self.peek() == Some('=') {
                    self.bump();
                    Token::GtEq
                } else {
                    Token::Gt
                }
            }
            other => return Err(error(format!("unexpected character '{other}'"))),
        };
        Ok(token)
    }

    /// Digits, then an optional `.digits`, then an optional exponent like `e-3`.
    fn number(&mut self, first: char) -> String {
        let mut text = first.to_string();
        self.digits(&mut text);
        if self.peek() == Some('.') && self.peek_second().is_some_and(|c| c.is_ascii_digit()) {
            text.push(self.bump().unwrap());
            self.digits(&mut text);
        }
        if matches!(self.peek(), Some('e' | 'E')) {
            let after = self.peek_second();
            let signed = matches!(after, Some('+' | '-'))
                && self
                    .chars
                    .get(self.pos + 2)
                    .is_some_and(|c| c.is_ascii_digit());
            if signed || after.is_some_and(|c| c.is_ascii_digit()) {
                text.push(self.bump().unwrap());
                if signed {
                    text.push(self.bump().unwrap());
                }
                self.digits(&mut text);
            }
        }
        text
    }

    fn digits(&mut self, text: &mut String) {
        while let Some(d) = self.peek().filter(char::is_ascii_digit) {
            text.push(d);
            self.bump();
        }
    }
}

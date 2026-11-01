//! A recursive-descent parser for statements. Expressions are in `expr.rs`.
//!
//! Each grammar rule is one method, so the code reads like the grammar:
//!
//! ```text
//! create := CREATE TABLE [IF NOT EXISTS] name ( column {, column} )
//! column := name type [PRIMARY KEY]
//! drop   := DROP TABLE [IF EXISTS] name
//! insert := INSERT INTO name [( name {, name} )] VALUES row {, row}
//! select := SELECT item {, item} [FROM name [when]] [WHERE expr]
//!           [ORDER BY expr [ASC|DESC] {, ...}] [LIMIT expr [OFFSET expr]]
//! when   := AS OF string | AS OF TXN number | HISTORY
//! update := UPDATE name SET name = expr {, name = expr} [WHERE expr]
//! delete := DELETE FROM name [WHERE expr]
//! txn    := BEGIN [TRANSACTION] | COMMIT | ROLLBACK
//! index  := CREATE INDEX [IF NOT EXISTS] name ON name ( name {, name} )
//!         | DROP INDEX [IF EXISTS] name
//! plan   := EXPLAIN (select | update | delete)
//! vacuum := VACUUM HISTORY BEFORE string
//! ```
//!
//! `BEGIN`, `COMMIT`, `ROLLBACK`, `TRANSACTION`, `INDEX`, `ON`, `EXPLAIN`,
//! `OF`, `TXN`, `HISTORY`, `VACUUM` and `BEFORE` aren't keywords, only words
//! with a meaning where no name can be. Reserving them would stop databases
//! that use them as names from opening, because the catalog parses its stored
//! `CREATE TABLE` text. `SELECT`'s own rules are in `select.rs`.

use super::ast::{ColumnDef, CreateIndex, CreateTable, Statement};
use super::token::{Keyword, Spanned, Token, tokenize};
use crate::error::{Error, Result};
use crate::types::DataType;

/// Parse any number of statements separated by `;`.
pub fn parse(sql: &str) -> Result<Vec<Statement>> {
    let mut parser = Parser {
        tokens: tokenize(sql)?,
        pos: 0,
    };
    let mut statements = Vec::new();
    loop {
        while parser.eat(&Token::Semicolon) {}
        if parser.peek() == &Token::Eof {
            return Ok(statements);
        }
        statements.push(parser.statement()?);
        if parser.peek() != &Token::Eof {
            parser.expect(&Token::Semicolon, "';' or end of input")?;
        }
    }
}

/// Parse exactly one statement (a trailing `;` is fine).
pub fn parse_one(sql: &str) -> Result<Statement> {
    let mut statements = parse(sql)?;
    match statements.len() {
        1 => Ok(statements.remove(0)),
        n => Err(Error::Syntax {
            message: format!("expected one statement, found {n}"),
            line: 1,
            column: 1,
        }),
    }
}

pub(super) struct Parser {
    tokens: Vec<Spanned>,
    pos: usize,
}

impl Parser {
    pub(super) fn peek(&self) -> &Token {
        &self.tokens[self.pos].token
    }

    /// Move past the current token (never past `Eof`).
    pub(super) fn advance(&mut self) {
        if self.peek() != &Token::Eof {
            self.pos += 1;
        }
    }

    pub(super) fn eat(&mut self, token: &Token) -> bool {
        let found = self.peek() == token;
        if found {
            self.advance();
        }
        found
    }

    pub(super) fn eat_keyword(&mut self, keyword: Keyword) -> bool {
        self.eat(&Token::Keyword(keyword))
    }

    pub(super) fn expect(&mut self, token: &Token, what: &str) -> Result<()> {
        if self.eat(token) {
            Ok(())
        } else {
            self.expected(what)
        }
    }

    pub(super) fn expect_keyword(&mut self, keyword: Keyword) -> Result<()> {
        self.expect(&Token::Keyword(keyword), &keyword.as_str())
    }

    /// A syntax error at the current token.
    pub(super) fn error<T>(&self, message: String) -> Result<T> {
        let at = &self.tokens[self.pos];
        Err(Error::Syntax {
            message,
            line: at.line,
            column: at.column,
        })
    }

    /// "expected X, found Y" at the current token.
    pub(super) fn expected<T>(&self, what: &str) -> Result<T> {
        self.error(format!("expected {what}, found {}", self.peek()))
    }

    pub(super) fn name(&mut self, what: &str) -> Result<String> {
        match self.peek().clone() {
            Token::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => self.expected(what),
        }
    }

    /// `item {, item}`, where each item is parsed by `one`.
    pub(super) fn comma_list<T>(
        &mut self,
        mut one: impl FnMut(&mut Self) -> Result<T>,
    ) -> Result<Vec<T>> {
        let mut items = vec![one(self)?];
        while self.eat(&Token::Comma) {
            items.push(one(self)?);
        }
        Ok(items)
    }

    fn statement(&mut self) -> Result<Statement> {
        match self.peek() {
            Token::Keyword(Keyword::Create) => self.create(),
            Token::Keyword(Keyword::Drop) => self.drop(),
            Token::Keyword(Keyword::Insert) => self.insert(),
            Token::Keyword(Keyword::Select) => self.select().map(Statement::Select),
            Token::Keyword(Keyword::Update) => self.update(),
            Token::Keyword(Keyword::Delete) => self.delete(),
            Token::Ident(word) if word == "begin" => {
                self.advance();
                self.eat_word("transaction");
                Ok(Statement::Begin)
            }
            Token::Ident(word) if word == "commit" => {
                self.advance();
                Ok(Statement::Commit)
            }
            Token::Ident(word) if word == "rollback" => {
                self.advance();
                Ok(Statement::Rollback)
            }
            Token::Ident(word) if word == "vacuum" => self.vacuum(),
            Token::Ident(word) if word == "explain" => {
                self.advance();
                match self.peek() {
                    Token::Keyword(Keyword::Select | Keyword::Update | Keyword::Delete) => {
                        Ok(Statement::Explain(Box::new(self.statement()?)))
                    }
                    _ => self.expected("SELECT, UPDATE or DELETE after EXPLAIN"),
                }
            }
            _ => self.expected(
                "a statement (CREATE, DROP, INSERT, SELECT, UPDATE, DELETE, BEGIN, COMMIT, \
                 ROLLBACK, EXPLAIN or VACUUM)",
            ),
        }
    }

    /// Move past the current token if it is the unreserved word `word`.
    pub(super) fn eat_word(&mut self, word: &str) -> bool {
        let found = matches!(self.peek(), Token::Ident(found) if found == word);
        if found {
            self.advance();
        }
        found
    }

    fn if_not_exists(&mut self) -> Result<bool> {
        let found = self.eat_keyword(Keyword::If);
        if found {
            self.expect_keyword(Keyword::Not)?;
            self.expect_keyword(Keyword::Exists)?;
        }
        Ok(found)
    }

    fn create(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Create)?;
        if self.eat_word("index") {
            return self.create_index();
        }
        self.expect_keyword(Keyword::Table)?;
        let if_not_exists = self.if_not_exists()?;
        let name = self.name("a table name")?;
        self.expect(&Token::LParen, "'('")?;
        let columns = self.comma_list(Self::column_def)?;
        self.expect(&Token::RParen, "')' or ','")?;
        Ok(Statement::CreateTable(CreateTable {
            name,
            if_not_exists,
            columns,
        }))
    }

    fn create_index(&mut self) -> Result<Statement> {
        let if_not_exists = self.if_not_exists()?;
        let name = self.name("an index name")?;
        if !self.eat_word("on") {
            return self.expected("ON");
        }
        let table = self.name("a table name")?;
        self.expect(&Token::LParen, "'('")?;
        let columns = self.comma_list(|p| p.name("a column name"))?;
        self.expect(&Token::RParen, "')' or ','")?;
        Ok(Statement::CreateIndex(CreateIndex {
            name,
            if_not_exists,
            table,
            columns,
        }))
    }

    fn column_def(&mut self) -> Result<ColumnDef> {
        let name = self.name("a column name")?;
        // Type names are ordinary words, so a column can still be called `text`.
        let data_type = match self.peek().clone() {
            Token::Ident(word) => match word.as_str() {
                "int" | "integer" => DataType::Int,
                "real" => DataType::Real,
                "text" => DataType::Text,
                "bool" | "boolean" => DataType::Bool,
                _ => {
                    return self
                        .error(format!("unknown type {word} (use INT, REAL, TEXT or BOOL)"));
                }
            },
            _ => return self.expected("a column type"),
        };
        self.advance();
        let primary_key = self.eat_keyword(Keyword::Primary);
        if primary_key {
            self.expect_keyword(Keyword::Key)?;
        }
        Ok(ColumnDef {
            name,
            data_type,
            primary_key,
        })
    }

    fn drop(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Drop)?;
        let index = self.eat_word("index");
        if !index {
            self.expect_keyword(Keyword::Table)?;
        }
        let if_exists = self.eat_keyword(Keyword::If);
        if if_exists {
            self.expect_keyword(Keyword::Exists)?;
        }
        if index {
            let name = self.name("an index name")?;
            return Ok(Statement::DropIndex { name, if_exists });
        }
        let name = self.name("a table name")?;
        Ok(Statement::DropTable { name, if_exists })
    }

    fn insert(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Insert)?;
        self.expect_keyword(Keyword::Into)?;
        let table = self.name("a table name")?;
        let columns = if self.eat(&Token::LParen) {
            let columns = self.comma_list(|p| p.name("a column name"))?;
            self.expect(&Token::RParen, "')' or ','")?;
            Some(columns)
        } else {
            None
        };
        self.expect_keyword(Keyword::Values)?;
        let rows = self.comma_list(|p| {
            p.expect(&Token::LParen, "'('")?;
            let row = p.comma_list(Self::expr)?;
            p.expect(&Token::RParen, "')' or ','")?;
            Ok(row)
        })?;
        Ok(Statement::Insert {
            table,
            columns,
            rows,
        })
    }

    fn update(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Update)?;
        let table = self.name("a table name")?;
        self.expect_keyword(Keyword::Set)?;
        let assignments = self.comma_list(|p| {
            let column = p.name("a column name")?;
            p.expect(&Token::Eq, "'='")?;
            Ok((column, p.expr()?))
        })?;
        let filter = self.where_clause()?;
        Ok(Statement::Update {
            table,
            assignments,
            filter,
        })
    }

    fn delete(&mut self) -> Result<Statement> {
        self.expect_keyword(Keyword::Delete)?;
        self.expect_keyword(Keyword::From)?;
        let table = self.name("a table name")?;
        let filter = self.where_clause()?;
        Ok(Statement::Delete { table, filter })
    }

    pub(super) fn where_clause(&mut self) -> Result<Option<super::ast::Expr>> {
        if self.eat_keyword(Keyword::Where) {
            Ok(Some(self.expr()?))
        } else {
            Ok(None)
        }
    }
}

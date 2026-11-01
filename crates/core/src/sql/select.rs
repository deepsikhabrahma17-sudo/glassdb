//! The parser's rules for `SELECT`, and the time-travel clauses that go with it.

use super::ast::{OrderBy, Select, SelectItem, Statement, When};
use super::parser::Parser;
use super::token::{Keyword, Token};
use crate::error::Result;
use crate::time::parse_timestamp;

impl Parser {
    pub(super) fn select(&mut self) -> Result<Select> {
        self.expect_keyword(Keyword::Select)?;
        let items = self.comma_list(Self::select_item)?;
        let mut when = When::Now;
        let from = if self.eat_keyword(Keyword::From) {
            let table = self.name("a table name")?;
            when = self.when()?;
            Some(table)
        } else {
            None
        };
        let filter = self.where_clause()?;
        let mut order_by = Vec::new();
        if self.eat_keyword(Keyword::Order) {
            self.expect_keyword(Keyword::By)?;
            order_by = self.comma_list(|p| {
                let expr = p.expr()?;
                let descending = p.eat_keyword(Keyword::Desc);
                if !descending {
                    p.eat_keyword(Keyword::Asc);
                }
                Ok(OrderBy { expr, descending })
            })?;
        }
        let (mut limit, mut offset) = (None, None);
        if self.eat_keyword(Keyword::Limit) {
            limit = Some(self.expr()?);
            if self.eat_keyword(Keyword::Offset) {
                offset = Some(self.expr()?);
            }
        }
        Ok(Select {
            items,
            from,
            when,
            filter,
            order_by,
            limit,
            offset,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem> {
        if self.eat(&Token::Star) {
            return Ok(SelectItem::Wildcard);
        }
        let expr = self.expr()?;
        // `expr AS name`, or just `expr name`.
        let alias = if self.eat_keyword(Keyword::As) || matches!(self.peek(), Token::Ident(_)) {
            Some(self.name("a column alias")?)
        } else {
            None
        };
        Ok(SelectItem::Expr { expr, alias })
    }

    /// What follows a table name: `AS OF ...`, `HISTORY`, or nothing.
    fn when(&mut self) -> Result<When> {
        if self.eat_word("history") {
            return self.nothing_more(When::History);
        }
        if !self.eat_keyword(Keyword::As) {
            return Ok(When::Now);
        }
        if !self.eat_word("of") {
            return self.expected("OF after AS");
        }
        if self.eat_word("txn") {
            return self.txn().and_then(|txn| self.nothing_more(When::Txn(txn)));
        }
        match self.peek() {
            Token::Text(_) => {
                let time = self.timestamp()?;
                self.nothing_more(When::Time(time))
            }
            _ => self.expected("TXN or a timestamp after AS OF"),
        }
    }

    /// `AS OF` and `HISTORY` each stand alone: there's one moment or all of them.
    fn nothing_more(&mut self, when: When) -> Result<When> {
        match self.peek() {
            Token::Ident(word) if word == "history" => {
                self.error("HISTORY can't follow AS OF".into())
            }
            Token::Keyword(Keyword::As) => {
                self.error("only one AS OF or HISTORY is allowed".into())
            }
            _ => Ok(when),
        }
    }

    fn txn(&mut self) -> Result<u64> {
        let id = match self.peek() {
            Token::Number(digits) => digits.parse().ok(),
            _ => None,
        };
        match id {
            Some(id) => {
                self.advance();
                Ok(id)
            }
            None => self.expected("a transaction id"),
        }
    }

    /// A timestamp in a string, in microseconds since the Unix epoch.
    fn timestamp(&mut self) -> Result<i64> {
        let parsed = match self.peek() {
            Token::Text(text) => parse_timestamp(text),
            _ => return self.expected("a timestamp"),
        };
        match parsed {
            Some(time) => {
                self.advance();
                Ok(time)
            }
            None => self.error(format!(
                "bad timestamp {} (use 'YYYY-MM-DD HH:MM:SS', in UTC or with +HH:MM)",
                self.peek()
            )),
        }
    }

    pub(super) fn vacuum(&mut self) -> Result<Statement> {
        self.advance();
        if !self.eat_word("history") {
            return self.expected("HISTORY after VACUUM");
        }
        if !self.eat_word("before") {
            return self.expected("BEFORE after VACUUM HISTORY");
        }
        let before = self.timestamp()?;
        Ok(Statement::Vacuum { before })
    }
}

//! Expression parsing, one method per precedence level, loosest first:
//!
//! ```text
//! expr       := or
//! or         := and {OR and}
//! and        := not {AND not}
//! not        := NOT not | comparison
//! comparison := additive [(= | <> | < | <= | > | >=) additive | IS [NOT] NULL]
//! additive   := term {(+ | -) term}
//! term       := unary {(* | / | %) unary}
//! unary      := - unary | primary
//! primary    := number | 'text' | TRUE | FALSE | NULL | name | ( expr )
//! ```
//!
//! A level only calls the next tighter one, which is what makes `*` bind
//! tighter than `+`, and `+` tighter than `=`.

use super::ast::{BinaryOp, Expr, UnaryOp};
use super::parser::Parser;
use super::token::{Keyword, Token};
use crate::error::Result;
use crate::types::Value;

fn binary(left: Expr, op: BinaryOp, right: Expr) -> Expr {
    Expr::Binary {
        left: Box::new(left),
        op,
        right: Box::new(right),
    }
}

impl Parser {
    pub(super) fn expr(&mut self) -> Result<Expr> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.eat_keyword(Keyword::Or) {
            left = binary(left, BinaryOp::Or, self.and()?);
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.not()?;
        while self.eat_keyword(Keyword::And) {
            left = binary(left, BinaryOp::And, self.not()?);
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_keyword(Keyword::Not) {
            let expr = Box::new(self.not()?);
            return Ok(Expr::Unary {
                op: UnaryOp::Not,
                expr,
            });
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Expr> {
        let left = self.additive()?;
        if self.eat_keyword(Keyword::Is) {
            let negated = self.eat_keyword(Keyword::Not);
            self.expect_keyword(Keyword::Null)?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        let op = match self.peek() {
            Token::Eq => BinaryOp::Eq,
            Token::NotEq => BinaryOp::NotEq,
            Token::Lt => BinaryOp::Lt,
            Token::LtEq => BinaryOp::LtEq,
            Token::Gt => BinaryOp::Gt,
            Token::GtEq => BinaryOp::GtEq,
            _ => return Ok(left),
        };
        self.advance();
        Ok(binary(left, op, self.additive()?))
    }

    fn additive(&mut self) -> Result<Expr> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Token::Plus => BinaryOp::Add,
                Token::Minus => BinaryOp::Sub,
                _ => return Ok(left),
            };
            self.advance();
            left = binary(left, op, self.term()?);
        }
    }

    fn term(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Token::Star => BinaryOp::Mul,
                Token::Slash => BinaryOp::Div,
                Token::Percent => BinaryOp::Rem,
                _ => return Ok(left),
            };
            self.advance();
            left = binary(left, op, self.unary()?);
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        if !self.eat(&Token::Minus) {
            return self.primary();
        }
        // A minus right before a number is part of the number. That is the
        // only way to write i64::MIN, whose positive half doesn't fit in i64.
        if let Token::Number(digits) = self.peek().clone() {
            let literal = self.number(&format!("-{digits}"))?;
            self.advance();
            return Ok(literal);
        }
        // Otherwise fold `-(5)` into the literal -5, and keep `-x` as an operation.
        match self.unary()? {
            Expr::Literal(Value::Int(n)) => match n.checked_neg() {
                Some(n) => Ok(Expr::Literal(Value::Int(n))),
                None => self.error("integer overflow".into()),
            },
            Expr::Literal(Value::Real(r)) => Ok(Expr::Literal(Value::Real(-r))),
            expr => Ok(Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(expr),
            }),
        }
    }

    fn primary(&mut self) -> Result<Expr> {
        let expr = match self.peek().clone() {
            Token::Number(digits) => self.number(&digits)?,
            Token::Text(text) => Expr::Literal(Value::Text(text)),
            Token::Keyword(Keyword::True) => Expr::Literal(Value::Bool(true)),
            Token::Keyword(Keyword::False) => Expr::Literal(Value::Bool(false)),
            Token::Keyword(Keyword::Null) => Expr::Literal(Value::Null),
            Token::Ident(name) => Expr::Column(name),
            Token::LParen => {
                self.advance();
                let inner = self.expr()?;
                self.expect(&Token::RParen, "')'")?;
                return Ok(inner);
            }
            _ => return self.expected("an expression"),
        };
        self.advance();
        Ok(expr)
    }

    /// A number literal: REAL if it has a `.` or an exponent, otherwise INT.
    fn number(&self, text: &str) -> Result<Expr> {
        if text.contains(['.', 'e', 'E']) {
            match text.parse::<f64>() {
                Ok(r) => Ok(Expr::Literal(Value::Real(r))),
                Err(_) => self.error(format!("bad number {text}")),
            }
        } else {
            match text.parse::<i64>() {
                Ok(n) => Ok(Expr::Literal(Value::Int(n))),
                Err(_) => self.error(format!("integer too large: {text}")),
            }
        }
    }
}

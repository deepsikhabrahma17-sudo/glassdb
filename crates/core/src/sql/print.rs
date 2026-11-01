//! Printing statements back as tidy SQL. Expressions print themselves in
//! `ast.rs`, next to the precedence rules that decide their parentheses.

use std::fmt;

use super::ast::{ColumnDef, OrderBy, SelectItem, Statement, When};
use crate::time::format_timestamp;

/// Write `items` separated by ", ".
fn list<T: fmt::Display>(f: &mut fmt::Formatter<'_>, items: &[T]) -> fmt::Result {
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            write!(f, ", ")?;
        }
        write!(f, "{item}")?;
    }
    Ok(())
}

impl fmt::Display for ColumnDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.name, self.data_type)?;
        if self.primary_key {
            write!(f, " PRIMARY KEY")?;
        }
        Ok(())
    }
}

impl fmt::Display for SelectItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectItem::Wildcard => write!(f, "*"),
            SelectItem::Expr { expr, alias: None } => write!(f, "{expr}"),
            SelectItem::Expr {
                expr,
                alias: Some(a),
            } => write!(f, "{expr} AS {a}"),
        }
    }
}

impl fmt::Display for OrderBy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}",
            self.expr,
            if self.descending { " DESC" } else { "" }
        )
    }
}

impl fmt::Display for Statement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Statement::CreateTable(c) => {
                let if_not_exists = if c.if_not_exists {
                    "IF NOT EXISTS "
                } else {
                    ""
                };
                write!(f, "CREATE TABLE {if_not_exists}{} (", c.name)?;
                list(f, &c.columns)?;
                write!(f, ")")
            }
            Statement::DropTable { name, if_exists } => {
                let if_exists = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP TABLE {if_exists}{name}")
            }
            Statement::Insert {
                table,
                columns,
                rows,
            } => {
                write!(f, "INSERT INTO {table}")?;
                if let Some(columns) = columns {
                    write!(f, " (")?;
                    list(f, columns)?;
                    write!(f, ")")?;
                }
                write!(f, " VALUES ")?;
                for (i, row) in rows.iter().enumerate() {
                    write!(f, "{}(", if i > 0 { ", " } else { "" })?;
                    list(f, row)?;
                    write!(f, ")")?;
                }
                Ok(())
            }
            Statement::Select(s) => {
                write!(f, "SELECT ")?;
                list(f, &s.items)?;
                if let Some(table) = &s.from {
                    write!(f, " FROM {table}")?;
                }
                match s.when {
                    When::Now => {}
                    When::Txn(txn) => write!(f, " AS OF TXN {txn}")?,
                    When::Time(time) => write!(f, " AS OF '{}'", format_timestamp(time))?,
                    When::History => write!(f, " HISTORY")?,
                }
                if let Some(filter) = &s.filter {
                    write!(f, " WHERE {filter}")?;
                }
                if !s.order_by.is_empty() {
                    write!(f, " ORDER BY ")?;
                    list(f, &s.order_by)?;
                }
                if let Some(limit) = &s.limit {
                    write!(f, " LIMIT {limit}")?;
                }
                if let Some(offset) = &s.offset {
                    write!(f, " OFFSET {offset}")?;
                }
                Ok(())
            }
            Statement::Update {
                table,
                assignments,
                filter,
            } => {
                write!(f, "UPDATE {table} SET ")?;
                for (i, (column, expr)) in assignments.iter().enumerate() {
                    write!(f, "{}{column} = {expr}", if i > 0 { ", " } else { "" })?;
                }
                if let Some(filter) = filter {
                    write!(f, " WHERE {filter}")?;
                }
                Ok(())
            }
            Statement::Delete { table, filter } => {
                write!(f, "DELETE FROM {table}")?;
                if let Some(filter) = filter {
                    write!(f, " WHERE {filter}")?;
                }
                Ok(())
            }
            Statement::Begin => write!(f, "BEGIN"),
            Statement::Commit => write!(f, "COMMIT"),
            Statement::Rollback => write!(f, "ROLLBACK"),
            Statement::CreateIndex(c) => {
                let if_not_exists = if c.if_not_exists {
                    "IF NOT EXISTS "
                } else {
                    ""
                };
                let columns = c.columns.join(", ");
                write!(
                    f,
                    "CREATE INDEX {if_not_exists}{} ON {} ({columns})",
                    c.name, c.table
                )
            }
            Statement::DropIndex { name, if_exists } => {
                let if_exists = if *if_exists { "IF EXISTS " } else { "" };
                write!(f, "DROP INDEX {if_exists}{name}")
            }
            Statement::Explain(inner) => write!(f, "EXPLAIN {inner}"),
            Statement::Vacuum { before } => {
                write!(f, "VACUUM HISTORY BEFORE '{}'", format_timestamp(*before))
            }
        }
    }
}

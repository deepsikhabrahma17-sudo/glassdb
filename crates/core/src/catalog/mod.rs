//! The catalog: the list of tables, kept in its own B+ tree so the schema
//! survives restarts.
//!
//! The catalog tree's root is always page 1 (a tree's root page never moves).
//! Each entry maps a table name to:
//!
//! | bytes | field                                         |
//! |-------|-----------------------------------------------|
//! | 0..4  | the table's root page (u32, little-endian)    |
//! | 4..   | its `CREATE TABLE` statement, as tidy SQL text |
//!
//! On open, the SQL is parsed again to rebuild the schema, as SQLite does.

#[cfg(test)]
mod tests;

use crate::btree::BTree;
use crate::error::{Error, Result};
use crate::pager::{PageId, Pager};
use crate::sql::{ColumnDef, CreateTable, Statement, parse_one};
use crate::types::DataType;

/// The catalog tree's root page.
pub const CATALOG_ROOT: PageId = 1;

/// One table: its name, where its rows live, and its columns.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    pub name: String,
    /// Root page of the B+ tree holding the table's rows.
    pub root: PageId,
    pub columns: Vec<ColumnDef>,
}

impl TableSchema {
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Which column is the primary key, if any.
    pub fn primary_key(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.primary_key)
    }

    /// The table's definition as tidy SQL.
    pub fn create_sql(&self) -> String {
        Statement::CreateTable(CreateTable {
            name: self.name.clone(),
            if_not_exists: false,
            columns: self.columns.clone(),
        })
        .to_string()
    }
}

pub struct Catalog {
    tree: BTree,
}

impl Catalog {
    /// Open the catalog, creating it if the database is brand new.
    pub fn open(pager: &mut Pager) -> Result<Catalog> {
        if pager.page_count() == 1 {
            // Only the header exists, so the first page allocated is page 1.
            let tree = BTree::create(pager)?;
            debug_assert_eq!(tree.root(), CATALOG_ROOT);
            return Ok(Catalog { tree });
        }
        let catalog = Catalog {
            tree: BTree::open(CATALOG_ROOT),
        };
        // Read page 1 now, so a file without a catalog fails at open.
        catalog.tree.iter(pager)?.next().transpose()?;
        Ok(catalog)
    }

    pub fn get(&self, pager: &mut Pager, name: &str) -> Result<Option<TableSchema>> {
        match self.tree.get(pager, name.as_bytes())? {
            Some(entry) => Ok(Some(decode_entry(name, &entry)?)),
            None => Ok(None),
        }
    }

    /// Every table, sorted by name.
    pub fn list(&self, pager: &mut Pager) -> Result<Vec<TableSchema>> {
        let mut tables = Vec::new();
        for entry in self.tree.iter(pager)? {
            let (name, value) = entry?;
            let name = String::from_utf8_lossy(&name).into_owned();
            tables.push(decode_entry(&name, &value)?);
        }
        Ok(tables)
    }

    /// Check a new table's definition, give it an empty B+ tree, and record it.
    pub fn create(&self, pager: &mut Pager, def: &CreateTable) -> Result<TableSchema> {
        let fail = |msg: String| Err(Error::Query(msg));
        if self.get(pager, &def.name)?.is_some() {
            return fail(format!("table {} already exists", def.name));
        }
        for (i, column) in def.columns.iter().enumerate() {
            if def.columns[..i].iter().any(|c| c.name == column.name) {
                return fail(format!("duplicate column {}", column.name));
            }
        }
        let keys: Vec<&ColumnDef> = def.columns.iter().filter(|c| c.primary_key).collect();
        if keys.len() > 1 {
            return fail("only one PRIMARY KEY column is allowed".into());
        }
        if let Some(key) = keys.first()
            && key.data_type != DataType::Int
        {
            return fail(format!(
                "PRIMARY KEY must be INT, but {} is {}",
                key.name, key.data_type
            ));
        }

        let table = BTree::create(pager)?;
        let schema = TableSchema {
            name: def.name.clone(),
            root: table.root(),
            columns: def.columns.clone(),
        };
        let mut entry = schema.root.to_le_bytes().to_vec();
        entry.extend(schema.create_sql().as_bytes());
        if let Err(err) = self.tree.insert(pager, schema.name.as_bytes(), &entry) {
            table.destroy(pager)?;
            return match err {
                Error::EntryTooLarge { .. } => {
                    fail(format!("table definition of {} is too long", def.name))
                }
                other => Err(other),
            };
        }
        Ok(schema)
    }

    /// Remove a table and free all of its pages. Returns it, if it existed.
    pub fn drop(&self, pager: &mut Pager, name: &str) -> Result<Option<TableSchema>> {
        let Some(schema) = self.get(pager, name)? else {
            return Ok(None);
        };
        BTree::open(schema.root).destroy(pager)?;
        self.tree.delete(pager, name.as_bytes())?;
        Ok(Some(schema))
    }
}

/// Rebuild a schema from its catalog entry.
fn decode_entry(name: &str, entry: &[u8]) -> Result<TableSchema> {
    let corrupt = |why: &str| Error::Corrupt(format!("catalog entry for {name}: {why}"));
    if entry.len() < 4 {
        return Err(corrupt("too short"));
    }
    let root = PageId::from_le_bytes(entry[..4].try_into().unwrap());
    let sql = std::str::from_utf8(&entry[4..]).map_err(|_| corrupt("SQL is not UTF-8"))?;
    match parse_one(sql) {
        Ok(Statement::CreateTable(def)) if def.name == name => Ok(TableSchema {
            name: def.name,
            root,
            columns: def.columns,
        }),
        _ => Err(corrupt("not a CREATE TABLE for this table")),
    }
}

//! The catalog: the list of tables and indexes, kept in its own B+ tree so the
//! schema survives restarts.
//!
//! The catalog tree's root is always page 1 (a tree's root page never moves).
//! Each entry maps a table or index name to (integers little-endian):
//!
//! | bytes  | field                                                       |
//! |--------|-------------------------------------------------------------|
//! | 0..4   | its root page                                               |
//! | 4..8   | a table's history tree root page (0 for an index)           |
//! | 8..16  | the transaction that created it                             |
//! | 16..24 | the transaction that dropped it (0 while it exists)         |
//! | 24..   | its `CREATE TABLE` or `CREATE INDEX` statement, as tidy SQL |
//!
//! The SQL is parsed again to rebuild the schema, as SQLite does, and also
//! says which kind of entry it is. Tables and indexes share one set of names.
//! A dropped table keeps its entry under a different key (see `dropped`).
//! Nothing is kept in memory, so a rollback needs no help here.

mod dropped;

#[cfg(test)]
mod tests;

use crate::btree::{BTree, MAX_ENTRY, MAX_KEY};
use crate::error::{Error, Result};
use crate::mvcc::TxnId;
use crate::pager::{PageId, Pager};
use crate::sql::{ColumnDef, CreateIndex, CreateTable, Statement, parse_one};
use crate::types::DataType;
pub use dropped::DroppedTable;
use dropped::{DROPPED_KEY_EXTRA, is_dropped_key, name_of_dropped_key};

/// Bytes before the SQL in a catalog entry.
const ENTRY_HEADER: usize = 24;

/// The catalog tree's root page.
pub const CATALOG_ROOT: PageId = 1;

/// One table: its name, where its rows live, and its columns.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSchema {
    pub name: String,
    /// Root page of the B+ tree holding the table's current rows.
    pub root: PageId,
    /// Root page of the B+ tree holding its replaced row versions.
    pub history_root: PageId,
    /// The transaction that created the table.
    pub created: TxnId,
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

/// One index: its name, its table, where its entries live, and its columns.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexSchema {
    pub name: String,
    pub table: String,
    /// Root page of the index's B+ tree.
    pub root: PageId,
    pub columns: Vec<String>,
}

impl IndexSchema {
    /// The index's definition as tidy SQL.
    pub fn create_sql(&self) -> String {
        Statement::CreateIndex(CreateIndex {
            name: self.name.clone(),
            if_not_exists: false,
            table: self.table.clone(),
            columns: self.columns.clone(),
        })
        .to_string()
    }
}

/// What a catalog entry describes.
enum Entry {
    Table(TableSchema),
    Index(IndexSchema),
    Dropped(DroppedTable),
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

    fn entry(&self, pager: &mut Pager, name: &str) -> Result<Option<Entry>> {
        match self.tree.get(pager, name.as_bytes())? {
            Some(entry) => Ok(Some(decode_entry(name, &entry)?)),
            None => Ok(None),
        }
    }

    fn entries(&self, pager: &mut Pager) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for entry in self.tree.iter(pager)? {
            let (key, value) = entry?;
            let name = match is_dropped_key(&key) {
                true => name_of_dropped_key(&key),
                false => String::from_utf8_lossy(&key).into_owned(),
            };
            entries.push(decode_entry(&name, &value)?);
        }
        Ok(entries)
    }

    /// The table called `name`.
    pub fn get(&self, pager: &mut Pager, name: &str) -> Result<Option<TableSchema>> {
        Ok(match self.entry(pager, name)? {
            Some(Entry::Table(table)) => Some(table),
            _ => None,
        })
    }

    /// The index called `name`.
    pub fn get_index(&self, pager: &mut Pager, name: &str) -> Result<Option<IndexSchema>> {
        Ok(match self.entry(pager, name)? {
            Some(Entry::Index(index)) => Some(index),
            _ => None,
        })
    }

    /// Every table, sorted by name.
    pub fn list(&self, pager: &mut Pager) -> Result<Vec<TableSchema>> {
        let tables = self.entries(pager)?.into_iter();
        Ok(tables
            .filter_map(|entry| match entry {
                Entry::Table(table) => Some(table),
                _ => None,
            })
            .collect())
    }

    /// Every index, sorted by name.
    pub fn indexes(&self, pager: &mut Pager) -> Result<Vec<IndexSchema>> {
        let indexes = self.entries(pager)?.into_iter();
        Ok(indexes
            .filter_map(|entry| match entry {
                Entry::Index(index) => Some(index),
                _ => None,
            })
            .collect())
    }

    /// The indexes on `table`, sorted by name.
    pub fn indexes_of(&self, pager: &mut Pager, table: &str) -> Result<Vec<IndexSchema>> {
        let mut indexes = self.indexes(pager)?;
        indexes.retain(|index| index.table == table);
        Ok(indexes)
    }

    fn check_name_is_free(&self, pager: &mut Pager, name: &str) -> Result<()> {
        let kind = match self.entry(pager, name)? {
            None | Some(Entry::Dropped(_)) => return Ok(()),
            Some(Entry::Table(_)) => "table",
            Some(Entry::Index(_)) => "index",
        };
        Err(Error::Query(format!("{kind} {name} already exists")))
    }

    /// Whether an entry for `name` fits, now and under the longer key it would
    /// get if it were dropped, so a later `DROP TABLE` can't fail for space.
    fn check_size(name: &str, sql: &str) -> Result<()> {
        let key = name.len() + DROPPED_KEY_EXTRA;
        if key > MAX_KEY || key + ENTRY_HEADER + sql.len() > MAX_ENTRY {
            return Err(Error::Query(format!(
                "the definition of {name} is too long"
            )));
        }
        Ok(())
    }

    /// Check a new index's definition against its table, without writing.
    pub fn check_new_index(
        &self,
        pager: &mut Pager,
        def: &CreateIndex,
        table: &TableSchema,
    ) -> Result<()> {
        self.check_name_is_free(pager, &def.name)?;
        for (i, column) in def.columns.iter().enumerate() {
            if table.column_index(column).is_none() {
                return Err(Error::Query(format!("no such column: {column}")));
            }
            if def.columns[..i].contains(column) {
                return Err(Error::Query(format!("duplicate column {column}")));
            }
        }
        Ok(())
    }

    /// Give a checked index an empty B+ tree and record it.
    pub fn create_index(
        &self,
        pager: &mut Pager,
        def: &CreateIndex,
        txn: TxnId,
    ) -> Result<IndexSchema> {
        let mut index = IndexSchema {
            name: def.name.clone(),
            table: def.table.clone(),
            root: 0,
            columns: def.columns.clone(),
        };
        let sql = index.create_sql();
        Self::check_size(&index.name, &sql)?;
        index.root = BTree::create(pager)?.root();
        let entry = encode_entry(index.root, 0, txn, 0, &sql);
        self.tree.insert(pager, index.name.as_bytes(), &entry)?;
        Ok(index)
    }

    /// Remove an index and free all of its pages. Returns it, if it existed.
    pub fn drop_index(&self, pager: &mut Pager, name: &str) -> Result<Option<IndexSchema>> {
        let Some(index) = self.get_index(pager, name)? else {
            return Ok(None);
        };
        BTree::open(index.root).destroy(pager)?;
        self.tree.delete(pager, name.as_bytes())?;
        Ok(Some(index))
    }

    /// Check a new table's definition, give it empty B+ trees, and record it.
    pub fn create(&self, pager: &mut Pager, def: &CreateTable, txn: TxnId) -> Result<TableSchema> {
        let fail = |msg: String| Err(Error::Query(msg));
        self.check_name_is_free(pager, &def.name)?;
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

        let mut schema = TableSchema {
            name: def.name.clone(),
            root: 0,
            history_root: 0,
            created: txn,
            columns: def.columns.clone(),
        };
        let sql = schema.create_sql();
        Self::check_size(&schema.name, &sql)?;
        schema.root = BTree::create(pager)?.root();
        schema.history_root = BTree::create(pager)?.root();
        let entry = encode_entry(schema.root, schema.history_root, txn, 0, &sql);
        self.tree.insert(pager, schema.name.as_bytes(), &entry)?;
        Ok(schema)
    }

    /// Verify the catalog's own tree. Returns how many pages it uses.
    pub fn check(&self, pager: &mut Pager) -> Result<usize> {
        let stats = self.tree.check(pager)?;
        Ok(stats.internal_nodes + stats.leaves)
    }
}

/// A catalog entry's bytes.
fn encode_entry(
    root: PageId,
    history_root: PageId,
    created: TxnId,
    dropped: TxnId,
    sql: &str,
) -> Vec<u8> {
    let mut entry = Vec::with_capacity(ENTRY_HEADER + sql.len());
    entry.extend(root.to_le_bytes());
    entry.extend(history_root.to_le_bytes());
    entry.extend(created.to_le_bytes());
    entry.extend(dropped.to_le_bytes());
    entry.extend(sql.as_bytes());
    entry
}

/// Rebuild a table or index from its catalog entry.
fn decode_entry(name: &str, entry: &[u8]) -> Result<Entry> {
    let corrupt = |why: &str| Error::Corrupt(format!("catalog entry for {name}: {why}"));
    if entry.len() < ENTRY_HEADER {
        return Err(corrupt("too short"));
    }
    let root = PageId::from_le_bytes(entry[..4].try_into().unwrap());
    let history_root = PageId::from_le_bytes(entry[4..8].try_into().unwrap());
    let created = TxnId::from_le_bytes(entry[8..16].try_into().unwrap());
    let dropped = TxnId::from_le_bytes(entry[16..24].try_into().unwrap());
    let sql = std::str::from_utf8(&entry[ENTRY_HEADER..]);
    let sql = sql.map_err(|_| corrupt("SQL is not UTF-8"))?;
    match parse_one(sql) {
        Ok(Statement::CreateTable(def)) if def.name == name => {
            let schema = TableSchema {
                name: def.name,
                root,
                history_root,
                created,
                columns: def.columns,
            };
            Ok(match dropped {
                0 => Entry::Table(schema),
                dropped => Entry::Dropped(DroppedTable { schema, dropped }),
            })
        }
        Ok(Statement::CreateIndex(def)) if def.name == name => Ok(Entry::Index(IndexSchema {
            name: def.name,
            table: def.table,
            root,
            columns: def.columns,
        })),
        _ => Err(corrupt("not a CREATE statement for this name")),
    }
}

use std::sync::Arc;

use super::*;
use crate::btree::BTree;
use crate::error::Error;
use crate::pager::Pager;
use crate::sql::{Statement, parse_one};
use crate::storage::MemStorage;
use crate::trace::NoopTracer;
use crate::types::DataType;

fn open(disk: &MemStorage) -> (Pager, Catalog) {
    let mut pager = Pager::open(Box::new(disk.clone()), 32, Arc::new(NoopTracer)).unwrap();
    let catalog = Catalog::open(&mut pager).unwrap();
    (pager, catalog)
}

fn create(pager: &mut Pager, catalog: &Catalog, sql: &str) -> crate::error::Result<TableSchema> {
    match parse_one(sql).unwrap() {
        Statement::CreateTable(def) => catalog.create(pager, &def, 1),
        other => panic!("not CREATE TABLE: {other:?}"),
    }
}

fn query_error(result: crate::error::Result<TableSchema>) -> String {
    match result {
        Err(Error::Query(msg)) => msg,
        other => panic!("expected a query error, got {other:?}"),
    }
}

#[test]
fn a_new_database_gets_its_catalog_on_page_1() {
    let (mut pager, catalog) = open(&MemStorage::new());
    assert_eq!(pager.page_count(), 2, "header + catalog root");
    assert!(catalog.list(&mut pager).unwrap().is_empty());
}

#[test]
fn create_get_and_list() {
    let (mut pager, catalog) = open(&MemStorage::new());
    let users = create(
        &mut pager,
        &catalog,
        "CREATE TABLE Users (id INT PRIMARY KEY, name TEXT)",
    )
    .unwrap();
    create(&mut pager, &catalog, "CREATE TABLE accounts (n REAL)").unwrap();

    assert_eq!(users.name, "users");
    assert_ne!(users.root, CATALOG_ROOT);
    assert_eq!(users.columns.len(), 2);
    assert_eq!(users.columns[0].data_type, DataType::Int);
    assert_eq!(users.primary_key(), Some(0));
    assert_eq!(users.column_index("name"), Some(1));
    assert_eq!(users.column_index("nope"), None);
    assert_eq!(
        users.create_sql(),
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)"
    );

    assert_eq!(catalog.get(&mut pager, "users").unwrap(), Some(users));
    assert_eq!(catalog.get(&mut pager, "nope").unwrap(), None);
    let names: Vec<String> = catalog
        .list(&mut pager)
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, vec!["accounts", "users"]);
}

#[test]
fn survives_close_and_reopen() {
    let disk = MemStorage::new();
    let (mut pager, catalog) = open(&disk);
    let users = create(
        &mut pager,
        &catalog,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT)",
    )
    .unwrap();
    pager.close().unwrap();

    let (mut pager, catalog) = open(&disk);
    assert_eq!(catalog.get(&mut pager, "users").unwrap(), Some(users));
}

#[test]
fn rejects_bad_table_definitions() {
    let (mut pager, catalog) = open(&MemStorage::new());
    create(&mut pager, &catalog, "CREATE TABLE t (a INT)").unwrap();
    let cases = [
        ("CREATE TABLE t (b INT)", "table t already exists"),
        ("CREATE TABLE d (a INT, a TEXT)", "duplicate column a"),
        (
            "CREATE TABLE d (a INT PRIMARY KEY, b INT PRIMARY KEY)",
            "only one PRIMARY KEY",
        ),
        (
            "CREATE TABLE d (a TEXT PRIMARY KEY)",
            "PRIMARY KEY must be INT, but a is TEXT",
        ),
    ];
    let pages = pager.page_count();
    for (sql, want) in cases {
        let msg = query_error(create(&mut pager, &catalog, sql));
        assert!(msg.contains(want), "{sql}: {msg}");
    }
    assert_eq!(
        pager.page_count(),
        pages,
        "rejected tables allocate nothing"
    );
}

#[test]
fn drop_frees_every_page_of_the_table() {
    let (mut pager, catalog) = open(&MemStorage::new());
    let t = create(&mut pager, &catalog, "CREATE TABLE t (a INT)").unwrap();
    let tree = BTree::open(t.root);
    for i in 0u32..200 {
        tree.insert(&mut pager, &i.to_be_bytes(), &[7; 300])
            .unwrap();
    }
    assert!(pager.page_count() > 10);

    assert_eq!(catalog.drop(&mut pager, "t", 1).unwrap(), Some(t));
    assert_eq!(catalog.get(&mut pager, "t").unwrap(), None);
    assert_eq!(catalog.drop(&mut pager, "t", 1).unwrap(), None);
    // Only the header and the catalog root are still in use.
    assert_eq!(pager.free_count(), pager.page_count() - 2);
}

#[test]
fn a_file_without_a_catalog_is_corrupt() {
    // Page 1 holds plain bytes, not a B+ tree node.
    let disk = MemStorage::new();
    let mut pager = Pager::open(Box::new(disk.clone()), 8, Arc::new(NoopTracer)).unwrap();
    let id = pager.allocate().unwrap();
    pager
        .write_page(id, &[b'x'; crate::pager::PAGE_SIZE])
        .unwrap();
    pager.close().unwrap();

    let mut pager = Pager::open(Box::new(disk), 8, Arc::new(NoopTracer)).unwrap();
    assert!(matches!(Catalog::open(&mut pager), Err(Error::Corrupt(_))));
}

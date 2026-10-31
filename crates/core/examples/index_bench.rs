//! The Phase 6 done-when check: on a table of a million rows in a real file,
//! a lookup through an index against the same lookup with a full scan.
//!
//! Build it in release mode, or the numbers mean little:
//!
//! ```text
//! cargo run --release -p glassdb-core --example index_bench
//! ```
//!
//! Pass a row count to try another size: `... --example index_bench -- 100000`.

use std::time::{Duration, Instant};

use glassdb_core::{Database, Output};

const DEFAULT_ROWS: u64 = 1_000_000;
/// Rows per transaction. Each one is a commit, which waits for the disk.
const BATCH: u64 = 20_000;
const SCANS: u64 = 5;
const LOOKUPS: u64 = 2_000;

/// A number in 0..rows, scattered so that `n` doesn't follow `id`.
fn scattered(i: u64, rows: u64) -> u64 {
    i.wrapping_mul(2_654_435_761) % rows
}

fn explain(db: &mut Database, sql: &str) -> glassdb_core::Result<()> {
    for line in db
        .execute(&format!("EXPLAIN {sql}"))?
        .to_string()
        .lines()
        .skip(1)
    {
        println!("    {line}");
    }
    Ok(())
}

/// Run `count` different lookups and return the average time for one.
fn average(db: &mut Database, count: u64, rows: u64, sql: impl Fn(u64) -> String) -> Duration {
    let queries: Vec<String> = (0..count)
        .map(|i| sql(scattered(i * 31 + 7, rows)))
        .collect();
    let started = Instant::now();
    for query in &queries {
        match db.execute(query) {
            // A lookup that finds nothing would flatter the index.
            Ok(Output::Rows { rows, .. }) => assert!(!rows.is_empty(), "no rows: {query}"),
            other => panic!("{query}: {other:?}"),
        }
    }
    started.elapsed() / count as u32
}

fn main() -> glassdb_core::Result<()> {
    let rows = std::env::args()
        .nth(1)
        .map_or(DEFAULT_ROWS, |n| n.parse().expect("a row count"));
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("bench.db");
    let mut db = Database::open(&path)?;
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT)")?;

    println!("Inserting {rows} rows...");
    let started = Instant::now();
    let mut next = 0;
    while next < rows {
        let end = (next + BATCH).min(rows);
        let values: Vec<String> = (next..end)
            .map(|i| format!("({i}, {}, 'row number {i}')", scattered(i, rows)))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))?;
        next = end;
    }
    println!("  took {:.1?}", started.elapsed());

    let by_n = |n: u64| format!("SELECT * FROM t WHERE n = {n}");

    println!("\nBefore CREATE INDEX:");
    explain(&mut db, &by_n(1))?;
    let scan = average(&mut db, SCANS, rows, by_n);
    println!("  average of {SCANS}: {scan:.2?} per lookup");

    println!("\nCREATE INDEX by_n ON t (n)...");
    let started = Instant::now();
    db.execute("CREATE INDEX by_n ON t (n)")?;
    println!("  took {:.1?}", started.elapsed());

    println!("\nAfter CREATE INDEX:");
    explain(&mut db, &by_n(1))?;
    let index = average(&mut db, LOOKUPS, rows, by_n);
    println!("  average of {LOOKUPS}: {index:.2?} per lookup");

    println!("\nThe primary key, for comparison:");
    let by_id = |id: u64| format!("SELECT * FROM t WHERE id = {id}");
    explain(&mut db, &by_id(1))?;
    let key = average(&mut db, LOOKUPS, rows, by_id);
    println!("  average of {LOOKUPS}: {key:.2?} per lookup");

    let size = std::fs::metadata(&path)?.len() as f64 / (1 << 20) as f64;
    println!(
        "\nThe index lookup was {:.0}x faster than the scan.",
        scan.as_secs_f64() / index.as_secs_f64()
    );
    println!("Database file: {size:.0} MB, with {rows} rows.");
    assert!(index < scan, "the index must be faster than the scan");
    db.close()
}

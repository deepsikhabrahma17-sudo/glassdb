//! A tour of time travel: every change is kept, so any past moment can be read.
//!
//! Run with: cargo run -p glassdb-core --example time_tour

use std::sync::Arc;

use glassdb_core::time::format_timestamp;
use glassdb_core::{Clock, Database, ManualClock, MemStorage, NoopTracer, Output, Result, Value};

const HOUR: i64 = 3_600_000_000;
const START: i64 = 1_793_534_400_000_000; // 2026-11-01 12:00:00 UTC

fn heading(text: &str) {
    println!("\n== {text}");
}

fn run(db: &mut Database, sql: &str) -> Result<()> {
    let output = db.execute(sql)?;
    println!("   {sql}\n     -> {output}");
    Ok(())
}

fn show(output: Output) {
    match output {
        Output::Rows { columns, rows } => {
            println!("     {}", columns.join(" | "));
            for row in rows {
                let cells: Vec<String> = row.iter().map(Value::to_string).collect();
                println!("     {}", cells.join(" | "));
            }
        }
        other => println!("     {other}"),
    }
}

fn query(db: &mut Database, sql: &str) -> Result<()> {
    println!("   {sql}");
    show(db.execute(sql)?);
    Ok(())
}

fn main() -> Result<()> {
    let mut db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        64,
        Arc::new(NoopTracer),
    )?;
    // A clock we can move, so the story's times are the same on every run.
    let clock = Arc::new(ManualClock::new(START));
    db.set_clock(clock.clone());

    heading("1. Every commit gets a number and a time");
    run(
        &mut db,
        "CREATE TABLE accounts (id INT PRIMARY KEY, owner TEXT, balance INT)",
    )?;
    clock.advance(HOUR);
    run(
        &mut db,
        "INSERT INTO accounts VALUES (1, 'ada', 100), (2, 'bob', 50)",
    )?;
    clock.advance(HOUR);
    run(&mut db, "UPDATE accounts SET balance = 80 WHERE id = 1")?;
    clock.advance(HOUR);
    run(&mut db, "DELETE FROM accounts WHERE id = 2")?;
    for commit in db.commits(10)?.iter().rev() {
        println!("   txn {}  {}", commit.txn, format_timestamp(commit.time));
    }

    heading("2. Rewind by transaction");
    query(&mut db, "SELECT * FROM accounts")?;
    query(&mut db, "SELECT * FROM accounts AS OF TXN 2")?;
    query(&mut db, "SELECT * FROM accounts AS OF TXN 3")?;

    heading("3. Rewind by time: the last commit at or before it counts");
    let sql = format!(
        "SELECT * FROM accounts AS OF '{}'",
        format_timestamp(START + 90 * HOUR / 60)
    );
    query(&mut db, &sql)?;

    heading("4. HISTORY lists every version, with the commits on either side");
    query(
        &mut db,
        "SELECT id, balance, created_txn, deleted_txn, deleted_at FROM accounts HISTORY",
    )?;

    heading("5. How it works: the past is a different scan of the same table");
    query(
        &mut db,
        "EXPLAIN SELECT * FROM accounts AS OF TXN 2 WHERE id = 1",
    )?;

    heading("6. A reader holds one moment while a transaction runs");
    let reader = db.reader()?;
    println!("   a reader is pinned at txn {}", reader.txn());
    run(&mut db, "BEGIN")?;
    run(&mut db, "UPDATE accounts SET balance = 0")?;
    println!("   the writer sees:");
    query(&mut db, "SELECT * FROM accounts")?;
    println!("   the reader sees, with the transaction still open:");
    show(reader.query("SELECT * FROM accounts")?);
    run(&mut db, "COMMIT")?;
    println!("   and after COMMIT, still:");
    show(reader.query("SELECT * FROM accounts")?);

    heading("7. A dropped table can still be read in the past");
    clock.advance(HOUR);
    run(&mut db, "DROP TABLE accounts")?;
    println!("   {}", db.execute("SELECT * FROM accounts").unwrap_err());
    query(&mut db, "SELECT * FROM accounts AS OF TXN 2")?;

    heading("8. VACUUM HISTORY frees old versions, and refuses to pass a reader");
    let report = db.check()?;
    println!(
        "   {} old versions and {} dropped table kept",
        report.old_versions, report.dropped_tables
    );
    clock.advance(HOUR);
    let vacuum = format!("VACUUM HISTORY BEFORE '{}'", format_timestamp(clock.now()));
    println!("   {}", db.execute(&vacuum).unwrap_err());
    drop(reader);
    run(&mut db, &vacuum)?;
    let report = db.check()?;
    println!(
        "   {} old versions and {} dropped tables kept, {} free pages",
        report.old_versions, report.dropped_tables, report.free_pages
    );
    println!(
        "   {}",
        db.execute("SELECT * FROM accounts AS OF TXN 2")
            .unwrap_err()
    );
    Ok(())
}

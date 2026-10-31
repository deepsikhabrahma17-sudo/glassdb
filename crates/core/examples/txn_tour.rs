//! A guided tour of transactions: several statements become one commit,
//! `ROLLBACK` leaves no trace, even after a crash, and a file has one owner.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p glassdb-core --example txn_tour
//! ```

use std::sync::Arc;

use glassdb_core::{Database, Error, Event, MemStorage, RecordingTracer};

/// A tiny cache, so a big transaction has to spill pages to the log.
const FRAMES: usize = 4;

struct Tour {
    db_file: MemStorage,
    log_file: MemStorage,
    tracer: Arc<RecordingTracer>,
}

impl Tour {
    fn open(&self) -> glassdb_core::Result<Database> {
        Database::open_with(
            Box::new(self.db_file.clone()),
            Box::new(self.log_file.clone()),
            FRAMES,
            self.tracer.clone(),
        )
    }

    fn log_size(&self) -> usize {
        self.log_file.to_bytes().len()
    }

    /// Print the transaction and commit events since the last call.
    fn show(&self) {
        let mut spilled = 0;
        for event in self.tracer.take() {
            match event {
                Event::WalAppend { .. } => spilled += 1,
                Event::TxnBegin => println!("   [event] BEGIN    changes now wait for COMMIT"),
                Event::WalCommit { frames } => {
                    let unit = if frames == 1 { "frame" } else { "frames" };
                    println!("   [event] LOG      commit record written, {frames} {unit}");
                    spilled = 0;
                }
                Event::TxnCommit => println!("   [event] COMMIT   safe from any crash"),
                Event::TxnRollback { frames } => {
                    println!("   [event] ROLLBACK {frames} spilled frames thrown away")
                }
                Event::WalRecovered {
                    transactions,
                    discarded,
                    ..
                } => println!(
                    "   [event] RECOVER  {transactions} transactions kept, \
                     {discarded} uncommitted frames thrown away"
                ),
                _ => {}
            }
        }
        if spilled > 0 {
            println!("   [event] LOG      {spilled} uncommitted pages spilled from the cache");
        }
    }
}

fn step(title: &str) {
    println!("\n== {title}");
}

fn print_rows(db: &mut Database) -> glassdb_core::Result<()> {
    let rows = db.execute("SELECT id, name FROM t ORDER BY id")?;
    println!("   {}", rows.to_string().replace('\n', "\n   "));
    Ok(())
}

/// Enough rows to overflow the cache.
fn big_insert() -> String {
    let pad = "x".repeat(300);
    let rows: Vec<String> = (100..150).map(|id| format!("({id}, '{pad}')")).collect();
    format!("INSERT INTO t VALUES {}", rows.join(", "))
}

fn main() -> glassdb_core::Result<()> {
    let tour = Tour {
        db_file: MemStorage::new(),
        log_file: MemStorage::new(),
        tracer: Arc::new(RecordingTracer::default()),
    };
    let mut db = tour.open()?;
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")?;
    db.execute("INSERT INTO t VALUES (1, 'ada'), (2, 'bob')")?;
    tour.tracer.take();

    step("1. Two statements, one commit");
    db.execute("BEGIN")?;
    db.execute("INSERT INTO t VALUES (3, 'cy')")?;
    db.execute("UPDATE t SET name = 'bobby' WHERE id = 2")?;
    tour.show();
    println!("   Nothing has committed yet, but this transaction sees its own changes:");
    print_rows(&mut db)?;
    db.execute("COMMIT")?;
    tour.show();

    step("2. A transaction bigger than the cache spills into the log");
    let before = tour.log_size();
    db.execute("BEGIN")?;
    db.execute(&big_insert())?;
    db.execute("DELETE FROM t WHERE id = 1")?;
    tour.show();
    println!(
        "   The log grew from {before} to {} bytes, all of it uncommitted.",
        tour.log_size()
    );

    step("3. ROLLBACK cuts the log back to the last commit");
    db.execute("ROLLBACK")?;
    tour.show();
    println!("   The log is {} bytes again.", tour.log_size());
    print_rows(&mut db)?;
    db.check()?;

    step("4. CREATE and DROP roll back too, because the catalog is just pages");
    db.execute("BEGIN")?;
    db.execute("CREATE TABLE ghost (a INT)")?;
    db.execute("DROP TABLE t")?;
    println!(
        "   Inside the transaction, the tables are: {:?}",
        names(&mut db)?
    );
    db.execute("ROLLBACK")?;
    println!("   After ROLLBACK: {:?}", names(&mut db)?);
    tour.tracer.take();

    step("5. Crash halfway through a transaction: reopening finds only commits");
    db.execute("BEGIN")?;
    db.execute(&big_insert())?;
    drop(db); // no close: as far as the database knows, the power just went out
    tour.tracer.take();
    let mut db = tour.open()?;
    tour.show();
    print_rows(&mut db)?;
    let report = db.check()?;
    println!(
        "   check: {} table, {} rows, {} pages, all accounted for",
        report.tables, report.rows, report.pages
    );
    db.close()?;

    step("6. One open at a time: a real file refuses a second opener");
    let dir = std::env::temp_dir().join(format!("glassdb-tour-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("tour.db");
    let first = Database::open(&path)?;
    match Database::open(&path) {
        Err(err @ Error::Locked) => println!("   second open: {err}"),
        Err(err) => return Err(err),
        Ok(_) => println!("   second open: unexpectedly allowed"),
    }
    first.close()?;
    Database::open(&path)?.close()?;
    println!("   After the first one closes, the file opens again.");
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

fn names(db: &mut Database) -> glassdb_core::Result<Vec<String>> {
    Ok(db.tables()?.into_iter().map(|t| t.name).collect())
}

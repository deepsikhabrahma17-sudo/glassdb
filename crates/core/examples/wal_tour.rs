//! A guided tour of the write-ahead log: commits go to the log first, a crash
//! keeps only whole transactions, and a checkpoint moves pages into the file.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p glassdb-core --example wal_tour
//! ```

use std::sync::Arc;

use glassdb_core::{Database, Event, MemStorage, RecordingTracer};

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
            64,
            self.tracer.clone(),
        )
    }

    /// Print the log and checkpoint events since the last call, and the file sizes.
    fn show(&self) {
        let mut frames = Vec::new();
        for event in self.tracer.take() {
            match event {
                Event::WalAppend { page } => frames.push(page.to_string()),
                Event::WalCommit { frames: count } => {
                    println!(
                        "   [event] LOG     pages {} appended, then a commit record ({})",
                        frames.join(", "),
                        plural(count as usize, "frame")
                    );
                    frames.clear();
                }
                Event::WalRecovered {
                    transactions,
                    frames,
                    discarded,
                } => println!(
                    "   [event] RECOVER {} kept ({}), {} after the last commit thrown away",
                    plural(transactions, "committed transaction"),
                    plural(frames, "frame"),
                    plural(discarded, "frame"),
                ),
                Event::Checkpoint { pages } => {
                    let pages = plural(pages as usize, "page");
                    println!(
                        "   [event] CHECKPOINT  {pages} copied into the main file, log emptied"
                    )
                }
                _ => {}
            }
        }
        println!(
            "   my.db: {} bytes   my.db-wal: {} bytes",
            self.db_file.to_bytes().len(),
            self.log_file.to_bytes().len()
        );
    }
}

fn plural(n: usize, word: &str) -> String {
    if n == 1 {
        format!("1 {word}")
    } else {
        format!("{n} {word}s")
    }
}

fn step(title: &str) {
    println!("\n== {title}");
}

fn main() -> glassdb_core::Result<()> {
    let tour = Tour {
        db_file: MemStorage::new(),
        log_file: MemStorage::new(),
        tracer: Arc::new(RecordingTracer::default()),
    };

    step("1. Create a database and a table: everything goes to the log");
    let mut db = tour.open()?;
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)")?;
    tour.show();
    println!("   The main file is still empty. Committed data lives in the log for now.");

    step("2. Insert a row: changed pages, then a commit record");
    db.execute("INSERT INTO t VALUES (1, 'safe')")?;
    tour.show();

    step("3. Reads find the newest version, even though it's only in the log");
    println!(
        "   {}",
        db.execute("SELECT * FROM t")?
            .to_string()
            .replace('\n', "\n   ")
    );
    tour.tracer.take();

    step("4. Insert another row, then crash while its commit record is being written");
    db.execute("INSERT INTO t VALUES (2, 'torn')")?;
    drop(db); // no close: as far as the database knows, the power just went out
    let mut bytes = tour.log_file.to_bytes();
    bytes.truncate(bytes.len() - 5); // the last 5 bytes never reached the disk
    let tour = Tour {
        log_file: MemStorage::from_bytes(bytes),
        ..tour
    };
    tour.tracer.take();
    tour.show();

    step("5. Reopen: committed transactions are kept, the torn one is gone");
    let mut db = tour.open()?;
    tour.show();
    println!(
        "   {}",
        db.execute("SELECT * FROM t")?
            .to_string()
            .replace('\n', "\n   ")
    );
    println!("   Row 2 never fully committed, so it is entirely absent, not half there.");
    let report = db.check()?;
    println!(
        "   check: {} table, {} row, {} pages, all accounted for",
        report.tables, report.rows, report.pages
    );

    step("6. More work, then a checkpoint moves it from the log into the main file");
    db.execute("INSERT INTO t VALUES (3, 'moved')")?;
    tour.show();
    db.checkpoint()?;
    tour.show();

    step("7. A clean close leaves everything in the main file and the log empty");
    db.close()?;
    tour.show();
    Ok(())
}

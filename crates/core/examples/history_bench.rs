//! What keeping history costs: the speed of writes, how much the file grows,
//! how fast past reads are, and how much vacuum gives back.
//!
//! Run with: cargo run --release -p glassdb-core --example history_bench

use std::time::Instant;

use glassdb_core::time::format_timestamp;
use glassdb_core::{Clock, Database, Output, Result, SystemClock};

const ROWS: usize = 100_000;
const BATCH: usize = 1_000;
const COMMITS: usize = 200;
const UPDATES_PER_COMMIT: usize = 100;

fn size_mb(path: &std::path::Path) -> f64 {
    std::fs::metadata(path).map_or(0.0, |m| m.len() as f64) / 1_048_576.0
}

fn count(output: Output) -> usize {
    match output {
        Output::Rows { rows, .. } => rows.len(),
        _ => 0,
    }
}

fn timed<T>(label: &str, work: impl FnOnce() -> Result<T>) -> Result<T> {
    let start = Instant::now();
    let out = work()?;
    println!("{label:<34} {:>10.2?}", start.elapsed());
    Ok(out)
}

/// xorshift64: which rows get updated.
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn main() -> Result<()> {
    let dir = std::env::temp_dir().join(format!("glassdb-history-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("bench.db");
    let mut db = Database::open(&path)?;
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT, pad TEXT)")?;

    timed(&format!("insert {ROWS} rows"), || {
        for start in (0..ROWS).step_by(BATCH) {
            let rows: Vec<String> = (start..start + BATCH)
                .map(|id| format!("({id}, 0, 'padding padding padding')"))
                .collect();
            db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", ")))?;
        }
        Ok(())
    })?;
    db.checkpoint()?;
    println!("{:<34} {:>7.1} MB", "file size", size_mb(&path));
    let loaded = db.latest_txn()?;

    let mut rng = 0x9E37_79B9_7F4A_7C15;
    let total = COMMITS * UPDATES_PER_COMMIT;
    timed(&format!("{total} updates, {COMMITS} commits"), || {
        for _ in 0..COMMITS {
            db.execute("BEGIN")?;
            for _ in 0..UPDATES_PER_COMMIT {
                let id = random(&mut rng) as usize % ROWS;
                db.execute(&format!("UPDATE t SET n = n + 1 WHERE id = {id}"))?;
            }
            db.execute("COMMIT")?;
        }
        Ok(())
    })?;
    timed("10000 deletes, one commit", || {
        db.execute("DELETE FROM t WHERE id < 10000")?;
        Ok(())
    })?;
    db.checkpoint()?;
    println!(
        "{:<34} {:>7.1} MB",
        "file size with history",
        size_mb(&path)
    );

    let scanned = timed("full scan, present", || {
        Ok(count(db.execute("SELECT * FROM t")?))
    })?;
    println!("{:<34} {scanned:>10}", "rows");
    let scanned = timed("full scan, AS OF", || {
        Ok(count(
            db.execute(&format!("SELECT * FROM t AS OF TXN {loaded}"))?,
        ))
    })?;
    println!("{:<34} {scanned:>10}", "rows");
    // Many lookups, so one cold page doesn't decide the number.
    timed("1000 lookups, present", || {
        for i in 0..1000 {
            let id = 10_000 + i * 90;
            count(db.execute(&format!("SELECT * FROM t WHERE id = {id}"))?);
        }
        Ok(())
    })?;
    timed("1000 lookups, AS OF", || {
        for i in 0..1000 {
            let id = 10_000 + i * 90;
            let sql = format!("SELECT * FROM t AS OF TXN {loaded} WHERE id = {id}");
            count(db.execute(&sql)?);
        }
        Ok(())
    })?;

    let now = format_timestamp(SystemClock.now());
    let output = timed("vacuum everything", || {
        db.execute(&format!("VACUUM HISTORY BEFORE '{now}'"))
    })?;
    println!("{output}");
    db.checkpoint()?;
    let report = db.check()?;
    println!(
        "{:<34} {:>7.1} MB, {} of {} pages free",
        "file size after vacuum",
        size_mb(&path),
        report.free_pages,
        report.pages
    );
    db.close()?;
    std::fs::remove_dir_all(&dir).unwrap();
    Ok(())
}

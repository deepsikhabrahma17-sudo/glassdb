//! The Phase 4 and 5 done-when checks: the real `glassdb` binary is killed
//! with SIGKILL at random moments, 200 times, on one growing database.
//!
//! After every kill the database must open and pass `check`, hold every
//! statement `glassdb` confirmed, and hold no half-applied statement. Nothing
//! from a rolled-back transaction may appear, and a committed transaction's
//! statements must all be there or all be gone. The file lock must also be
//! free again, or the open right after the kill would be refused.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use glassdb_core::{Database, Output, Value};

const KILLS: usize = 200;
/// Every INSERT adds this many rows, so a torn one shows as a smaller group.
const BATCH: usize = 5;
const COUNTERS: usize = 5;
/// The batch number of rows that are always rolled back.
const GHOST: i64 = -1;

/// xorshift64: where each kill lands. The seed is printed so a failure can
/// be repeated.
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).unwrap() {
        Output::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn create_tables(path: &Path) {
    let mut db = Database::open(path).unwrap();
    db.execute("CREATE TABLE ledger (id INT PRIMARY KEY, batch INT, pad TEXT)")
        .unwrap();
    db.execute("CREATE TABLE counters (id INT PRIMARY KEY, n INT)")
        .unwrap();
    // `check` proves after every kill that these still match their tables.
    db.execute("CREATE INDEX ledger_by_batch ON ledger (batch)")
        .unwrap();
    db.execute("CREATE INDEX counters_by_n ON counters (n)")
        .unwrap();
    db.execute("CREATE TABLE scratch (n INT)").unwrap();
    db.execute("CREATE INDEX scratch_by_n ON scratch (n)")
        .unwrap();
    db.execute("CREATE TABLE left_side (batch INT)").unwrap();
    db.execute("CREATE TABLE right_side (batch INT)").unwrap();
    for _ in 0..COUNTERS {
        db.execute("INSERT INTO counters (n) VALUES (0)").unwrap();
    }
    db.close().unwrap();
}

/// The statements of one batch: an INSERT and an UPDATE of every counter,
/// then a transaction that rolls back and one that commits.
fn batch_sql(batch: usize) -> String {
    let pad = "p".repeat(200);
    let rows: Vec<String> = (0..BATCH)
        .map(|_| format!("(NULL, {batch}, '{pad}')"))
        .collect();
    format!(
        "INSERT INTO ledger VALUES {rows};\n\
         UPDATE counters SET n = n + 1;\n\
         BEGIN;\n\
         INSERT INTO ledger VALUES (NULL, {GHOST}, 'ghost');\n\
         UPDATE counters SET n = n + 1000000;\n\
         DELETE FROM ledger WHERE batch < {batch};\n\
         ROLLBACK;\n\
         BEGIN;\n\
         INSERT INTO left_side VALUES ({batch});\n\
         INSERT INTO right_side VALUES ({batch});\n\
         INSERT INTO scratch VALUES ({batch});\n\
         DELETE FROM scratch WHERE n < {batch};\n\
         COMMIT;\n",
        rows = rows.join(", ")
    )
}

/// Run `glassdb` until it has confirmed something, let it go on for
/// `delay`, then kill it. Returns how many INSERTs and UPDATEs it confirmed.
fn run_and_kill(path: &Path, first_batch: usize, delay: Duration) -> (usize, usize) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_glassdb"))
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn glassdb");

    let mut stdin = child.stdin.take().unwrap();
    let writer = thread::spawn(move || {
        for batch in first_batch.. {
            let mut text = batch_sql(batch);
            if batch % 3 == 0 {
                text.push_str(".checkpoint\n"); // so kills also land in checkpoints
            }
            // Fails with a broken pipe once the child is dead.
            if stdin.write_all(text.as_bytes()).is_err() {
                return;
            }
        }
    });

    let inserts = Arc::new(AtomicUsize::new(0));
    let updates = Arc::new(AtomicUsize::new(0));
    let stdout = child.stdout.take().unwrap();
    let reader = {
        let (inserts, updates) = (inserts.clone(), updates.clone());
        thread::spawn(move || {
            // Only statements outside a transaction count as confirmed here.
            let mut in_transaction = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.starts_with("transaction started") {
                    in_transaction = true;
                } else if line.starts_with("rolled back") || line.starts_with("committed") {
                    in_transaction = false;
                } else if in_transaction {
                    continue;
                } else if line.starts_with("inserted ") {
                    inserts.fetch_add(1, Ordering::SeqCst);
                } else if line.starts_with("updated ") {
                    updates.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
    };

    while inserts.load(Ordering::SeqCst) == 0 {
        thread::sleep(Duration::from_millis(1));
    }
    thread::sleep(delay);
    child.kill().unwrap(); // SIGKILL: no chance to clean up
    child.wait().unwrap();
    writer.join().unwrap();
    reader.join().unwrap();
    (
        inserts.load(Ordering::SeqCst),
        updates.load(Ordering::SeqCst),
    )
}

#[test]
fn survives_200_random_kills() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.db");
    create_tables(&path);

    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        | 1;
    println!("kill seed: {seed}");
    let mut rng = seed;

    let mut next_batch = 0; // the first batch number not yet known to be in
    let mut counter_value = 0; // what every counter held after the last round
    let mut recovered = 0;
    for round in 0..KILLS {
        let delay = Duration::from_micros(random(&mut rng) % 30_000);
        let (inserts, updates) = run_and_kill(&path, next_batch, delay);
        let at = format!("round {round} (seed {seed})");

        let mut db = Database::open(&path).unwrap_or_else(|e| panic!("{at}: open failed: {e}"));
        recovered += db.recovered();
        db.check()
            .unwrap_or_else(|e| panic!("{at}: check failed: {e}"));

        // A committed transaction wrote both sides, or neither.
        let sides = |db: &mut Database, table: &str| rows(db, &format!("SELECT * FROM {table}"));
        assert_eq!(
            sides(&mut db, "left_side"),
            sides(&mut db, "right_side"),
            "{at}: a committed transaction is half there"
        );

        // Every batch must be whole, and nothing rolled back may appear.
        let mut groups: BTreeMap<i64, usize> = BTreeMap::new();
        for row in rows(&mut db, "SELECT batch FROM ledger") {
            let Value::Int(batch) = row[0] else {
                panic!("{at}: bad batch value")
            };
            assert_ne!(batch, GHOST, "{at}: a rolled-back row came back");
            *groups.entry(batch).or_default() += 1;
        }
        for (batch, count) in &groups {
            assert_eq!(*count, BATCH, "{at}: batch {batch} is half-written");
        }

        // Every confirmed INSERT is there, and at most one more (the one in
        // flight when the kill landed), with no gaps.
        let present = groups.len();
        let confirmed = next_batch + inserts;
        assert!(
            present == confirmed || present == confirmed + 1,
            "{at}: {confirmed} batches confirmed, {present} present"
        );
        assert_eq!(
            groups
                .keys()
                .next_back()
                .map(|b| *b as usize + 1)
                .unwrap_or(0),
            present
        );

        // UPDATE touches every counter, so they must all agree. An UPDATE
        // only follows its INSERT, so it can be confirmed, in flight or absent.
        let counters: Vec<i64> = rows(&mut db, "SELECT n FROM counters")
            .iter()
            .map(|row| match row[0] {
                Value::Int(n) => n,
                _ => panic!("{at}: bad counter"),
            })
            .collect();
        assert_eq!(counters.len(), COUNTERS);
        assert!(
            counters.iter().all(|n| *n == counters[0]),
            "{at}: UPDATE half-applied: {counters:?}"
        );
        let value = counters[0] as usize;
        assert!(
            value == counter_value + updates || value == counter_value + updates + 1,
            "{at}: counters at {value}, was {counter_value}, {updates} confirmed"
        );

        next_batch = present;
        counter_value = value;
        db.close().unwrap();
    }
    println!("{KILLS} kills survived; {recovered} transactions recovered from the log");
    assert!(recovered > 0, "the kills never left anything in the log");
}

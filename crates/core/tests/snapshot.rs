//! The Phase 7 done-when check for readers, with real threads: a reader keeps
//! a stable snapshot while a writer runs, and never waits for it to commit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use glassdb_core::{Database, Output, Reader, Value};

const ACCOUNTS: i64 = 40;
const OPENING_BALANCE: i64 = 100;
const TOTAL: i64 = ACCOUNTS * OPENING_BALANCE;
const TRANSFERS: u64 = 1_500;

fn open() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Database::open(dir.path().join("bank.db")).unwrap();
    db.execute("CREATE TABLE accounts (id INT PRIMARY KEY, balance INT)")
        .unwrap();
    db.execute("CREATE INDEX by_balance ON accounts (balance)")
        .unwrap();
    let rows: Vec<String> = (1..=ACCOUNTS)
        .map(|id| format!("({id}, {OPENING_BALANCE})"))
        .collect();
    db.execute(&format!("INSERT INTO accounts VALUES {}", rows.join(", ")))
        .unwrap();
    (dir, db)
}

fn balances(reader: &Reader) -> Vec<i64> {
    let Output::Rows { rows, .. } = reader.query("SELECT balance FROM accounts").unwrap() else {
        panic!("expected rows")
    };
    rows.iter()
        .map(|row| match row[0] {
            Value::Int(n) => n,
            _ => panic!("bad balance"),
        })
        .collect()
}

/// xorshift64, so a run is repeatable.
fn random(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

/// Move money between two accounts, taking several statements to do it, so
/// the total is wrong between them and only a commit makes it right again.
fn transfer(db: &mut Database, rng: &mut u64) {
    let from = random(rng) % ACCOUNTS as u64 + 1;
    let to = random(rng) % ACCOUNTS as u64 + 1;
    let amount = random(rng) % 7 + 1;
    db.execute("BEGIN").unwrap();
    db.execute(&format!(
        "UPDATE accounts SET balance = balance - {amount} WHERE id = {from}"
    ))
    .unwrap();
    db.execute(&format!(
        "UPDATE accounts SET balance = balance + {amount} WHERE id = {to}"
    ))
    .unwrap();
    // One in five changes its mind, leaving nothing behind.
    db.execute(if random(rng).is_multiple_of(5) {
        "ROLLBACK"
    } else {
        "COMMIT"
    })
    .unwrap();
}

#[test]
fn a_pinned_reader_sees_one_moment_throughout() {
    let (_dir, mut db) = open();
    let done = Arc::new(AtomicBool::new(false));

    // Pinned before the writer starts, so every read must see the opening
    // balances, however far the writer gets.
    let pinned: Vec<_> = (0..3)
        .map(|_| {
            let reader = db.reader().unwrap();
            let done = done.clone();
            thread::spawn(move || {
                let mut reads = 0u64;
                while !done.load(Ordering::SeqCst) || reads == 0 {
                    let found = balances(&reader);
                    assert_eq!(found, vec![OPENING_BALANCE; ACCOUNTS as usize]);
                    reads += 1;
                }
                reads
            })
        })
        .collect();

    // Moves on to each new commit in turn. Every moment it sees must add up,
    // which it wouldn't if a read ever caught a transfer half-way.
    let following = {
        let mut reader = db.reader().unwrap();
        let done = done.clone();
        thread::spawn(move || {
            let mut moments = 0u64;
            let mut last = reader.txn();
            while !done.load(Ordering::SeqCst) {
                let first = balances(&reader);
                assert_eq!(first.iter().sum::<i64>(), TOTAL);
                assert_eq!(balances(&reader), first, "a reader's moment moved");
                reader.refresh().unwrap();
                assert!(reader.txn() >= last, "a reader went back in time");
                moments += u64::from(reader.txn() > last);
                last = reader.txn();
            }
            moments
        })
    };

    let mut rng = 0x1234_5678_9ABC_DEF1;
    for i in 0..TRANSFERS {
        transfer(&mut db, &mut rng);
        if i % 300 == 0 {
            db.checkpoint().unwrap();
        }
    }
    done.store(true, Ordering::SeqCst);

    for reader in pinned {
        assert!(reader.join().unwrap() > 0);
    }
    let moments = following.join().unwrap();
    println!("the following reader saw {moments} different moments");
    assert!(moments > 0, "the reader never saw the writer move");
    db.check().unwrap();
}

#[test]
fn a_reader_finishes_while_a_transaction_stays_open() {
    let (_dir, mut db) = open();
    let reader = db.reader().unwrap();
    db.execute("BEGIN").unwrap();
    db.execute("UPDATE accounts SET balance = 0 WHERE id = 1")
        .unwrap();

    // If reads queued behind the open transaction, this would never answer,
    // because COMMIT below only runs after the answer arrives.
    let (answer, answered) = mpsc::channel();
    let thread = thread::spawn(move || {
        answer.send(balances(&reader)).unwrap();
    });
    let found = answered
        .recv_timeout(std::time::Duration::from_secs(30))
        .expect("the reader waited for the transaction");
    assert_eq!(found, vec![OPENING_BALANCE; ACCOUNTS as usize]);
    thread.join().unwrap();
    db.execute("COMMIT").unwrap();
}

#[test]
fn many_readers_each_keep_their_own_moment() {
    let (_dir, mut db) = open();
    let mut rng = 7;
    let mut pinned = Vec::new();
    for _ in 0..30 {
        let reader = db.reader().unwrap();
        pinned.push((balances(&reader), reader));
        for _ in 0..5 {
            transfer(&mut db, &mut rng);
        }
    }
    let threads: Vec<_> = pinned
        .into_iter()
        .map(|(expected, reader)| thread::spawn(move || assert_eq!(balances(&reader), expected)))
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
}

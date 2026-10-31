//! Simulated power cuts.
//!
//! Killing a process can't show what a power cut does, because the operating
//! system still finishes every write it was handed. This fake disk does:
//! anything written since the last sync may survive whole, survive torn
//! part-way through, or be lost, chosen at random per write.
//!
//! A fixed workload runs and the power is cut at every single write, truncate
//! and sync in turn. After each cut the database must reopen, pass `check`,
//! and hold exactly the state after the last confirmed statement, or after
//! the one in flight. It must never hold a partly applied statement, or
//! anything from a transaction that rolled back or never committed.

use std::io;
use std::sync::{Arc, Mutex};

use glassdb_core::{Database, Error, MemStorage, NoopTracer, Result, Storage};

const SEEDS: [u64; 3] = [1, 0x9E37_79B9, 0xDEAD_BEEF];
/// A small cache, so big statements spill uncommitted pages to the log.
const FRAMES: usize = 4;

#[derive(Clone)]
enum Op {
    Write { offset: usize, data: Vec<u8> },
    Truncate(usize),
}

#[derive(Default)]
struct FileState {
    /// What is guaranteed to survive: the bytes as of the last sync.
    durable: Vec<u8>,
    /// What the program sees.
    current: Vec<u8>,
    /// Changes since the last sync, in order.
    unsynced: Vec<Op>,
}

struct DiskState {
    files: [FileState; 2],
    /// Writes, truncates and syncs left before the power goes out.
    ops_left: u64,
    ops_done: u64,
    power_cut: bool,
    rng: u64,
}

impl DiskState {
    fn random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    /// Count one operation; the power goes out when the budget runs out.
    fn tick(&mut self) -> Result<()> {
        if !self.power_cut && self.ops_left == 0 {
            self.cut_power();
        }
        if self.power_cut {
            return Err(power_cut());
        }
        self.ops_left -= 1;
        self.ops_done += 1;
        Ok(())
    }

    /// Each unsynced change survives, tears, or is lost, independently.
    fn cut_power(&mut self) {
        self.power_cut = true;
        for i in 0..self.files.len() {
            let ops = std::mem::take(&mut self.files[i].unsynced);
            let mut image = self.files[i].durable.clone();
            for op in ops {
                let roll = self.random();
                match op {
                    Op::Write { offset, data } => match roll % 4 {
                        0 => {}
                        1 => {
                            let keep = (self.random() % data.len().max(1) as u64) as usize;
                            apply_write(&mut image, offset, &data[..keep]);
                        }
                        _ => apply_write(&mut image, offset, &data),
                    },
                    Op::Truncate(len) => {
                        if roll.is_multiple_of(2) {
                            image.resize(len, 0);
                        }
                    }
                }
            }
            self.files[i].durable = image;
        }
    }
}

fn apply_write(image: &mut Vec<u8>, offset: usize, data: &[u8]) {
    if image.len() < offset + data.len() {
        image.resize(offset + data.len(), 0);
    }
    image[offset..offset + data.len()].copy_from_slice(data);
}

fn power_cut() -> Error {
    Error::Io(io::Error::other("power cut"))
}

#[derive(Clone)]
struct Disk(Arc<Mutex<DiskState>>);

impl Disk {
    fn new(images: [Vec<u8>; 2], ops_left: u64, rng: u64) -> Disk {
        let [db, wal] = images;
        let file = |bytes: Vec<u8>| FileState {
            current: bytes.clone(),
            durable: bytes,
            unsynced: Vec::new(),
        };
        Disk(Arc::new(Mutex::new(DiskState {
            files: [file(db), file(wal)],
            ops_left,
            ops_done: 0,
            power_cut: false,
            rng: rng | 1,
        })))
    }

    fn file(&self, which: usize) -> Box<dyn Storage> {
        Box::new(DiskFile {
            disk: self.clone(),
            which,
        })
    }

    fn open(&self) -> Result<Database> {
        Database::open_with(self.file(0), self.file(1), FRAMES, Arc::new(NoopTracer))
    }

    /// What is left on the platters after the power cut.
    fn survivors(&self) -> [Vec<u8>; 2] {
        let state = self.0.lock().unwrap();
        [
            state.files[0].durable.clone(),
            state.files[1].durable.clone(),
        ]
    }

    fn ops_done(&self) -> u64 {
        self.0.lock().unwrap().ops_done
    }
}

struct DiskFile {
    disk: Disk,
    which: usize,
}

impl Storage for DiskFile {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let state = self.disk.0.lock().unwrap();
        if state.power_cut {
            return Err(power_cut());
        }
        let offset = offset as usize;
        match state.files[self.which]
            .current
            .get(offset..offset + buf.len())
        {
            Some(bytes) => buf.copy_from_slice(bytes),
            None => return Err(Error::Io(io::ErrorKind::UnexpectedEof.into())),
        }
        Ok(())
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        let mut state = self.disk.0.lock().unwrap();
        state.tick()?;
        let file = &mut state.files[self.which];
        apply_write(&mut file.current, offset as usize, data);
        file.unsynced.push(Op::Write {
            offset: offset as usize,
            data: data.to_vec(),
        });
        Ok(())
    }

    fn truncate(&mut self, len: u64) -> Result<()> {
        let mut state = self.disk.0.lock().unwrap();
        state.tick()?;
        let file = &mut state.files[self.which];
        file.current.resize(len as usize, 0);
        file.unsynced.push(Op::Truncate(len as usize));
        Ok(())
    }

    fn size(&mut self) -> Result<u64> {
        let state = self.disk.0.lock().unwrap();
        if state.power_cut {
            return Err(power_cut());
        }
        Ok(state.files[self.which].current.len() as u64)
    }

    fn sync(&mut self) -> Result<()> {
        let mut state = self.disk.0.lock().unwrap();
        state.tick()?;
        let file = &mut state.files[self.which];
        file.durable = file.current.clone();
        file.unsynced.clear();
        Ok(())
    }
}

enum Step {
    Sql(String),
    Checkpoint,
}

/// A fixed workload touching every kind of change the engine can make.
fn workload() -> Vec<Step> {
    let note = "n".repeat(100);
    let mut steps = vec![
        Step::Sql("CREATE TABLE a (id INT PRIMARY KEY, grp INT, n INT, note TEXT)".into()),
        Step::Sql("CREATE TABLE b (x INT PRIMARY KEY, y TEXT)".into()),
        Step::Sql("CREATE INDEX a_by_grp ON a (grp, n)".into()),
        Step::Sql("CREATE INDEX b_by_y ON b (y)".into()),
    ];
    for g in 0..18 {
        let rows: Vec<String> = (0..4)
            .map(|_| format!("(NULL, {g}, 0, '{note}')"))
            .collect();
        steps.push(Step::Sql(format!(
            "INSERT INTO a VALUES {}",
            rows.join(", ")
        )));
        if g % 3 == 0 {
            steps.push(Step::Sql("UPDATE a SET n = n + 1".into()));
        }
        if g % 4 == 1 {
            // Bigger than the cache, so pages spill before the commit.
            let rows: Vec<String> = (0..120).map(|_| format!("(NULL, '{note}')")).collect();
            steps.push(Step::Sql(format!(
                "INSERT INTO b VALUES {}",
                rows.join(", ")
            )));
        }
        if g % 5 == 2 {
            steps.push(Step::Sql(format!("DELETE FROM a WHERE grp = {}", g - 1)));
        }
        if g % 6 == 3 {
            steps.push(Step::Checkpoint);
        }
        if g % 9 == 4 {
            steps.push(Step::Sql("DROP TABLE b".into()));
            steps.push(Step::Sql(
                "CREATE TABLE b (x INT PRIMARY KEY, y TEXT)".into(),
            ));
            steps.push(Step::Sql("CREATE INDEX b_by_y ON b (y)".into()));
        }
        if g % 11 == 6 {
            steps.push(Step::Sql("DROP INDEX a_by_grp".into()));
            steps.push(Step::Sql("CREATE INDEX a_by_grp ON a (grp, n)".into()));
        }
        if g % 7 == 2 {
            // Spill a long transaction, roll it back, then commit a shorter
            // one over the same stretch of the log.
            let rows: Vec<String> = (0..120).map(|_| "(NULL, 'ghost')".to_string()).collect();
            steps.push(Step::Sql("BEGIN".into()));
            steps.push(Step::Sql(format!(
                "INSERT INTO b VALUES {}",
                rows.join(", ")
            )));
            steps.push(Step::Sql("UPDATE a SET note = 'ghost'".into()));
            steps.push(Step::Sql("ROLLBACK".into()));
            steps.push(Step::Sql("BEGIN".into()));
            steps.push(Step::Sql(format!(
                "INSERT INTO a VALUES (NULL, {g}, 1, 'x')"
            )));
            steps.push(Step::Sql("UPDATE a SET n = n + 1".into()));
            steps.push(Step::Sql("COMMIT".into()));
        }
        if g % 8 == 5 {
            steps.push(Step::Sql("BEGIN".into()));
            steps.push(Step::Sql("CREATE TABLE ghost (y TEXT)".into()));
            steps.push(Step::Sql("INSERT INTO ghost VALUES ('ghost')".into()));
            steps.push(Step::Sql("DROP INDEX a_by_grp".into()));
            steps.push(Step::Sql("DROP TABLE b".into()));
            steps.push(Step::Sql("ROLLBACK".into()));
        }
    }
    steps
}

fn run(db: &mut Database, step: &Step) -> Result<()> {
    match step {
        Step::Sql(sql) => db.execute(sql).map(|_| ()),
        Step::Checkpoint => db.checkpoint(),
    }
}

/// Every table's name and rows, as text.
fn contents(db: &mut Database) -> String {
    let mut out = String::new();
    for table in db.tables().unwrap() {
        let rows = db
            .execute(&format!("SELECT * FROM {}", table.name))
            .unwrap();
        out.push_str(&format!("{}:\n{rows}\n", table.name));
    }
    out
}

/// Reopen what survived, verify it, and return its contents.
fn reopen_and_check(images: [Vec<u8>; 2]) -> String {
    let [db, wal] = images;
    let mut db = Database::open_with(
        Box::new(MemStorage::from_bytes(db)),
        Box::new(MemStorage::from_bytes(wal)),
        FRAMES,
        Arc::new(NoopTracer),
    )
    .expect("the database must open after a power cut");
    db.check()
        .expect("the database must be intact after a power cut");
    contents(&mut db)
}

#[test]
fn power_cuts_at_every_disk_operation() {
    let steps = workload();

    // A run with no power cut gives the state a crash after each step must
    // leave. Inside a transaction, that is still the last committed state.
    let disk = Disk::new([Vec::new(), Vec::new()], u64::MAX, 1);
    let mut db = disk.open().unwrap();
    let mut states = vec![contents(&mut db)];
    for step in &steps {
        run(&mut db, step).unwrap();
        let state = if db.in_transaction() {
            states.last().unwrap().clone()
        } else {
            contents(&mut db)
        };
        states.push(state);
    }
    db.close().unwrap();
    let total_ops = disk.ops_done();
    println!("{} steps, {total_ops} disk operations", steps.len());

    let mut outcomes = [0usize; 2];
    for seed in SEEDS {
        for cut in 0..total_ops {
            let disk = Disk::new([Vec::new(), Vec::new()], cut, seed ^ cut);
            let mut confirmed = 0;
            if let Ok(mut db) = disk.open() {
                for step in &steps {
                    if run(&mut db, step).is_err() {
                        break;
                    }
                    confirmed += 1;
                }
            }

            // Sometimes the power also fails during recovery.
            let again = Disk::new(disk.survivors(), seed % 40, seed ^ cut ^ 0xABCD);
            let images = match again.open() {
                Ok(_) => again.survivors(),
                Err(_) => again.survivors(),
            };

            let found = reopen_and_check(images);
            assert!(
                !found.contains("ghost"),
                "seed {seed}, cut at operation {cut}: a rolled-back change came back"
            );
            if found == states[confirmed] {
                outcomes[0] += 1;
            } else if states.get(confirmed + 1) == Some(&found) {
                outcomes[1] += 1; // the commit landed just before the power went
            } else {
                panic!(
                    "seed {seed}, cut at operation {cut}: after {confirmed} confirmed steps \
                     the database holds neither that state nor the next"
                );
            }
        }
    }
    println!(
        "{} clean, {} with the in-flight step kept",
        outcomes[0], outcomes[1]
    );
    assert!(outcomes[0] > 0 && outcomes[1] > 0, "{outcomes:?}");
}

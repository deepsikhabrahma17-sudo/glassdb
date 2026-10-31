//! A guided tour of indexes and the planner: what an index holds, how the
//! planner picks a way to find rows, and how many pages each way reads.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p glassdb-core --example index_tour
//! ```

use std::sync::Arc;

use glassdb_core::{Database, Event, MemStorage, Output, RecordingTracer};

const ROWS: usize = 5_000;

struct Tour {
    db: Database,
    tracer: Arc<RecordingTracer>,
}

impl Tour {
    fn explain(&mut self, sql: &str) -> glassdb_core::Result<()> {
        println!("   {sql}");
        let plan = self.db.execute(&format!("EXPLAIN {sql}"))?.to_string();
        for line in plan.lines().skip(1) {
            println!("     {line}");
        }
        Ok(())
    }

    /// Run a query and report how many pages it touched.
    fn measure(&mut self, sql: &str) -> glassdb_core::Result<()> {
        self.tracer.take();
        let rows = match self.db.execute(sql)? {
            Output::Rows { rows, .. } => rows.len(),
            other => panic!("expected rows, got {other:?}"),
        };
        let pages = self
            .tracer
            .take()
            .iter()
            .filter(|e| matches!(e, Event::CacheHit { .. } | Event::CacheMiss { .. }))
            .count();
        let unit = if rows == 1 { "row" } else { "rows" };
        println!("   {rows} {unit}, {pages} pages read: {sql}");
        Ok(())
    }
}

fn step(title: &str) {
    println!("\n== {title}");
}

fn main() -> glassdb_core::Result<()> {
    let tracer = Arc::new(RecordingTracer::default());
    let db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        64,
        tracer.clone(),
    )?;
    let mut tour = Tour { db, tracer };

    step("1. A table of 5,000 people. Only `id` has a shortcut so far");
    tour.db
        .execute("CREATE TABLE people (id INT PRIMARY KEY, age INT, city TEXT)")?;
    tour.db.execute("BEGIN")?;
    for batch in (0..ROWS).step_by(500) {
        let rows: Vec<String> = (batch..batch + 500)
            .map(|i| format!("({i}, {}, 'city {}')", 18 + i % 60, i % 40))
            .collect();
        tour.db
            .execute(&format!("INSERT INTO people VALUES {}", rows.join(", ")))?;
    }
    tour.db.execute("COMMIT")?;
    tour.explain("SELECT * FROM people WHERE id = 42")?;
    tour.explain("SELECT * FROM people WHERE age = 30")?;
    tour.measure("SELECT * FROM people WHERE id = 42")?;
    tour.measure("SELECT * FROM people WHERE age = 30")?;

    step("2. Create indexes: each is a B+ tree of its own");
    tour.db.execute("CREATE INDEX by_age ON people (age)")?;
    tour.db
        .execute("CREATE INDEX by_city_age ON people (city, age)")?;
    let check = tour.db.check()?;
    println!(
        "   check: {} rows, {} pages, and every index holds exactly its table's rows",
        check.rows, check.pages
    );

    step("3. Equal columns narrow the search, then one range");
    tour.explain("SELECT * FROM people WHERE age = 30")?;
    tour.explain("SELECT * FROM people WHERE age > 70 AND age <= 72")?;
    tour.explain("SELECT * FROM people WHERE city = 'city 3' AND age > 50")?;
    tour.measure("SELECT * FROM people WHERE city = 'city 3' AND age > 50")?;

    step("4. What the index can't answer is still filtered, row by row");
    tour.explain("SELECT * FROM people WHERE age = 30 AND id > 100")?;

    step("5. Some questions have no shortcut, and the answer stays right");
    tour.explain("SELECT * FROM people WHERE age = 30 OR age = 31")?;
    tour.explain("SELECT * FROM people WHERE city = 'city 3' OR id = 1")?;

    step("6. An UPDATE uses the same plan, then keeps the indexes in step");
    tour.explain("UPDATE people SET age = 99 WHERE age = 30")?;
    tour.db
        .execute("UPDATE people SET age = 99 WHERE age = 30")?;
    tour.measure("SELECT * FROM people WHERE age = 30")?;
    tour.measure("SELECT * FROM people WHERE age = 99")?;
    tour.db.check()?;

    step("7. Rolled back with everything else");
    tour.db.execute("BEGIN")?;
    tour.db.execute("DROP INDEX by_age")?;
    println!("   Inside the transaction, by_age is gone:");
    tour.explain("SELECT * FROM people WHERE age = 99")?;
    tour.db.execute("ROLLBACK")?;
    println!("   After ROLLBACK:");
    tour.explain("SELECT * FROM people WHERE age = 99")?;

    tour.db.check()?;
    Ok(())
}

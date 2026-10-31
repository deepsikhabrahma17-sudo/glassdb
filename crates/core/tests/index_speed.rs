//! An index reads far fewer pages than a scan. Pages are counted rather than
//! timed, so this can't fail because a machine is busy. The timed version on
//! a million rows is `examples/index_bench.rs`.

use std::sync::Arc;

use glassdb_core::{Database, Event, MemStorage, Output, RecordingTracer};

const ROWS: usize = 20_000;

/// Pages a statement touched, cached or not, and how many rows it returned.
fn pages_read(db: &mut Database, tracer: &RecordingTracer, sql: &str) -> (usize, usize) {
    tracer.take();
    let rows = match db.execute(sql).unwrap() {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    let pages = tracer
        .take()
        .iter()
        .filter(|e| matches!(e, Event::CacheHit { .. } | Event::CacheMiss { .. }))
        .count();
    (pages, rows)
}

#[test]
fn an_index_reads_far_fewer_pages_than_a_scan() {
    let tracer = Arc::new(RecordingTracer::default());
    let mut db = Database::open_with(
        Box::new(MemStorage::new()),
        Box::new(MemStorage::new()),
        64,
        tracer.clone(),
    )
    .unwrap();
    db.execute("CREATE TABLE t (id INT PRIMARY KEY, n INT, s TEXT)")
        .unwrap();
    db.execute("BEGIN").unwrap();
    for batch in (0..ROWS).step_by(1000) {
        let rows: Vec<String> = (batch..batch + 1000)
            .map(|i| format!("({i}, {}, 'row number {i}')", i * 7 % ROWS))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", rows.join(", ")))
            .unwrap();
    }
    db.execute("COMMIT").unwrap();

    let by_n = "SELECT * FROM t WHERE n = 7000";
    let (scan_pages, scan_rows) = pages_read(&mut db, &tracer, by_n);

    db.execute("CREATE INDEX by_n ON t (n)").unwrap();
    let (index_pages, index_rows) = pages_read(&mut db, &tracer, by_n);
    let (key_pages, key_rows) = pages_read(&mut db, &tracer, "SELECT * FROM t WHERE id = 1000");

    println!("scan {scan_pages} pages, index {index_pages}, primary key {key_pages}");
    assert_eq!(scan_rows, 1);
    assert_eq!(index_rows, 1);
    assert_eq!(key_rows, 1);
    assert!(scan_pages > 200, "the table is {scan_pages} pages");
    assert!(
        index_pages <= 10,
        "an index lookup read {index_pages} pages"
    );
    assert!(key_pages <= 6, "a key lookup read {key_pages} pages");
}

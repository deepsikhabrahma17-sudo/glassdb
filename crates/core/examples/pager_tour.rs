//! A guided tour of the pager: watch the cache, the free list and the file
//! format at work, one step at a time.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p glassdb-core --example pager_tour
//! ```

use std::sync::Arc;

use glassdb_core::{Event, FileStorage, PAGE_SIZE, Page, Pager, RecordingTracer};

/// A deliberately tiny cache, so evictions show up after a few pages.
const FRAMES: usize = 3;

fn main() -> glassdb_core::Result<()> {
    let path = std::env::temp_dir().join("glassdb-pager-tour.db");
    let _ = std::fs::remove_file(&path); // start fresh each run
    println!("Database file: {}", path.display());
    println!("Cache size: {FRAMES} frames of {PAGE_SIZE} bytes");

    let tracer = Arc::new(RecordingTracer::default());
    let open = |tracer: &Arc<RecordingTracer>| {
        let storage = FileStorage::open(&path)?;
        Pager::open(Box::new(storage), FRAMES, tracer.clone())
    };

    step("1. Create a new database (only the header page exists)");
    let mut pager = open(&tracer)?;
    show(&tracer);
    println!("   page count = {}", pager.page_count());

    step("2. Allocate 4 pages and write a message into each");
    for _ in 0..4 {
        let id = pager.allocate()?;
        pager.write_page(id, &text_page(&format!("hello from page {id}")))?;
    }
    show(&tracer);
    println!("   page count = {}  (4 pages + header)", pager.page_count());
    println!("   Only 3 frames, so page 1 was evicted and written to disk to make room.");

    step("3. Read page 4 (still cached) and then page 1 (not cached)");
    println!("   page 4 says: {:?}", read_text(&mut pager, 4)?);
    println!("   page 1 says: {:?}", read_text(&mut pager, 1)?);
    show(&tracer);

    step("4. Free page 2, then allocate again: page 2 is reused, zeroed");
    pager.free(2)?;
    println!("   free pages = {}", pager.free_count());
    let reused = pager.allocate()?;
    println!("   allocate() returned page {reused}");
    println!(
        "   page {reused} now says: {:?}",
        read_text(&mut pager, reused)?
    );
    show(&tracer);

    step("5. Close (flush dirty pages, then the header, then sync)");
    pager.close()?;
    show(&tracer);

    step("6. Reopen the file and read page 3 back");
    let mut pager = open(&tracer)?;
    println!("   page count = {}", pager.page_count());
    println!("   page 3 says: {:?}", read_text(&mut pager, 3)?);
    show(&tracer);

    step("7. The raw header bytes (page 0) on disk");
    let raw = std::fs::read(&path)?;
    println!(
        "   file size = {} bytes = {} pages",
        raw.len(),
        raw.len() / PAGE_SIZE
    );
    println!("   {}  magic \"GLASSDB\\0\"", hex(&raw[0..8]));
    println!("   {}  format version", hex(&raw[8..10]));
    println!(
        "   {}  page size (little-endian: 0x1000 = 4096)",
        hex(&raw[10..12])
    );
    println!("   {}  page count", hex(&raw[12..16]));
    println!("   {}  free-list head (0 = empty)", hex(&raw[16..20]));
    println!("   {}  free count", hex(&raw[20..24]));
    Ok(())
}

/// A zeroed page with `text` at the start.
fn text_page(text: &str) -> Page {
    let mut page = [0u8; PAGE_SIZE];
    page[..text.len()].copy_from_slice(text.as_bytes());
    page
}

/// The text at the start of a page, up to the first zero byte.
fn read_text(pager: &mut Pager, id: u32) -> glassdb_core::Result<String> {
    let page = pager.read_page(id)?;
    let end = page.iter().position(|&b| b == 0).unwrap_or(PAGE_SIZE);
    Ok(String::from_utf8_lossy(&page[..end]).into_owned())
}

fn step(title: &str) {
    println!("\n== {title}");
}

/// Print and clear the events recorded since the last call.
fn show(tracer: &RecordingTracer) {
    for event in tracer.take() {
        let line = match event {
            Event::CacheHit { page } => format!("cache HIT    page {page}"),
            Event::CacheMiss { page } => format!("cache MISS   page {page}"),
            Event::CacheEvict { page, dirty } => {
                let state = if dirty { "dirty, must save" } else { "clean" };
                format!("cache EVICT  page {page} ({state})")
            }
            Event::PageRead { page } => format!("disk  READ   page {page}"),
            Event::PageWrite { page } => format!("disk  WRITE  page {page}"),
            // B+ tree events; the pager alone never produces them.
            other => format!("{other:?}"),
        };
        println!("   [event] {line}");
    }
}

fn hex(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{:<24}", parts.join(" "))
}

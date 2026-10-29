//! A guided tour of the B+ tree: watch it split, grow, scan, borrow, merge
//! and shrink, with the tree drawn after every step.
//!
//! Run it with:
//!
//! ```text
//! cargo run -p glassdb-core --example btree_tour
//! ```

use std::ops::Bound;
use std::sync::Arc;

use glassdb_core::{BTree, Event, MemStorage, Pager, RecordingTracer};

/// Big values, so a leaf holds only 4 entries and splits come quickly.
fn value() -> Vec<u8> {
    vec![b'.'; 900]
}

fn key(i: u32) -> Vec<u8> {
    format!("k{i:02}").into_bytes()
}

fn main() -> glassdb_core::Result<()> {
    let tracer = Arc::new(RecordingTracer::default());
    let mut pager = Pager::open(Box::new(MemStorage::new()), 64, tracer.clone())?;
    let tree = BTree::create(&mut pager)?;
    println!("Each value is 900 bytes, so one 4 KB leaf holds 4 entries.");
    tracer.take();

    step("1. Insert k00..k03: they fit in the root leaf");
    for i in 0..4 {
        tree.insert(&mut pager, &key(i), &value())?;
    }
    show(&mut pager, &tree, &tracer)?;

    step("2. Insert k04: the leaf overflows, splits, and the root grows a level");
    tree.insert(&mut pager, &key(4), &value())?;
    show(&mut pager, &tree, &tracer)?;

    step("3. Insert k05..k11: more leaf splits, each adding a separator to the root");
    for i in 5..12 {
        tree.insert(&mut pager, &key(i), &value())?;
    }
    show(&mut pager, &tree, &tracer)?;

    step("4. Look up k07: one root-to-leaf walk");
    let found = tree.get(&mut pager, &key(7))?.is_some();
    let visits: Vec<String> = tracer
        .take()
        .into_iter()
        .filter_map(|e| match e {
            Event::NodeVisit { page } => Some(format!("page {page}")),
            _ => None,
        })
        .collect();
    println!("   found = {found}, visited {}", visits.join(" → "));

    step("5. Range scan k03 ..= k08: walk down once, then along the leaf chain");
    let range = tree.range(
        &mut pager,
        Bound::Included(&key(3)),
        Bound::Included(&key(8)),
    )?;
    let mut keys = Vec::new();
    for entry in range {
        keys.push(String::from_utf8_lossy(&entry?.0).into_owned());
    }
    println!("   {}", keys.join(" "));
    tracer.take();

    step("6. Fill page 5 (add k06a, k06b), then delete k09..k11 from page 6");
    println!("   Page 6 drops to one entry (underfull). Merging it into full page 5");
    println!("   would not fit in one page, so the two share their entries instead.");
    tree.insert(&mut pager, b"k06a", &value())?;
    tree.insert(&mut pager, b"k06b", &value())?;
    for i in 9..12 {
        tree.delete(&mut pager, &key(i))?;
    }
    show(&mut pager, &tree, &tracer)?;

    step("7. Delete k00..k02: the first leaf empties out and merges with its neighbour");
    for i in 0..3 {
        tree.delete(&mut pager, &key(i))?;
    }
    show(&mut pager, &tree, &tracer)?;

    step("8. Delete everything else: leaves merge until the root shrinks back to one leaf");
    let rest: Vec<Vec<u8>> = tree
        .iter(&mut pager)?
        .map(|e| e.map(|(k, _)| k))
        .collect::<Result<_, _>>()?;
    for k in &rest {
        tree.delete(&mut pager, k)?;
    }
    show(&mut pager, &tree, &tracer)?;
    println!(
        "   pages in file: {}, free for reuse: {} (only the header and root are in use)",
        pager.page_count(),
        pager.free_count()
    );
    Ok(())
}

fn step(title: &str) {
    println!("\n== {title}");
}

/// Print the structural events since the last call, then draw the tree.
fn show(pager: &mut Pager, tree: &BTree, tracer: &RecordingTracer) -> glassdb_core::Result<()> {
    for event in tracer.take() {
        let line = match event {
            Event::NodeSplit {
                page,
                new_page,
                separator,
            } => format!(
                "SPLIT   page {page} → right half to page {new_page}, separator {:?} goes up",
                String::from_utf8_lossy(&separator)
            ),
            Event::RootGrow { root, left, right } => {
                format!("GROW    root page {root} now points to pages {left} and {right}")
            }
            Event::NodeBorrow { left, right } => {
                format!("BORROW  pages {left} and {right} shared their entries evenly")
            }
            Event::NodeMerge { left, right } => {
                format!("MERGE   page {right} merged into page {left} and freed")
            }
            Event::RootShrink { root, child } => {
                format!("SHRINK  page {child} pulled up into root page {root} and freed")
            }
            _ => continue, // node visits and cache traffic: too noisy here
        };
        println!("   [event] {line}");
    }
    let stats = tree.check(pager)?;
    println!(
        "   height {}, {} leaves, {} entries:",
        stats.height, stats.leaves, stats.entries
    );
    for line in tree.dump(pager)?.lines() {
        println!("     {line}");
    }
    tracer.take(); // drop the visits made by check and dump
    Ok(())
}

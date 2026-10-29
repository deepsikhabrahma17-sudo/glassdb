# GlassDB — Design

This file records the on-disk format and the design decisions behind it.
It grows with each build phase.

## Workspace layout

| Crate | Package name | Kind | Purpose |
| --- | --- | --- | --- |
| `crates/core` | `glassdb-core` | library | The engine: pager, B+ tree, WAL, MVCC, SQL, planner, tracer |
| `crates/cli` | `glassdb` | binary | The `glassdb` prompt (`cargo install glassdb`) |

A browser crate (`crates/wasm`) and the visualizer (`web/`) are added when the
glass-box phase starts, so early phases don't carry a WebAssembly toolchain.

**Decision: no database or storage crates.** Pages, the cache, the B+ tree, the
log and recovery are all written in this repo. Small helper crates are allowed
where they are not storage: `rustyline` (prompt), `crc32fast` (checksums),
`proptest`/`criterion` (testing).

## CLI

```text
glassdb <database-file>
```

- With no file argument, it prints usage to stderr and exits with code 2.
- It prints a banner with the version and file name, then shows a `glassdb> ` prompt.
- Lines are trimmed. A blank line does nothing. `.exit` or `.quit` leaves.
  Ctrl-D (end of input) also leaves with exit code 0. Ctrl-C clears the current line.
- Prompt logic lives in `crates/cli/src/repl.rs` as a pure function
  (`handle_line`), so it is unit tested without a terminal. `main.rs` only does I/O.
- `rustyline` provides line editing and history on a terminal, and plain line
  reads when stdin is piped. The integration tests depend on that piped mode.

The CLI opens the file through the pager, creating a one-page database if it is
missing, and shows the page count in the banner. A file that isn't a GlassDB
database is reported and left untouched (exit code 1). On exit the pager is
closed (flushed). Input is still only echoed until the SQL layer exists.

## Storage

The pager reads and writes through a `Storage` trait (`read_at`, `write_at`,
`size`, `sync`), not through `File` directly.

- `FileStorage` is a real file, used by the CLI.
- `MemStorage` is a `Vec<u8>` behind `Arc<Mutex<…>>`. Clones share the same bytes, so a
  test can close a pager and reopen "the same disk". The browser build will use it too.

**Decision:** keeping storage behind a trait keeps unit tests fast and makes the
WASM build possible without a file system. The crash tests can also inject
failures through it later.

## File format

Every page is **4096 bytes**. Page `n` starts at byte `n × 4096`. All integers are
**little-endian**.

### Page 0: header

| Bytes | Field | Notes |
| --- | --- | --- |
| 0–8 | magic | `GLASSDB\0` |
| 8–10 | format version | `1` |
| 10–12 | page size | `4096`; a file with another size is refused |
| 12–16 | page count | Includes the header page, so an empty database has 1 |
| 16–20 | free-list head | First free page id; `0` = list is empty |
| 20–24 | free count | Number of pages on the free list |
| 24–4096 | zero | Reserved |

On open, the header is rejected if any of these hold:
- the magic, version or page size is wrong
- the page count is 0
- the free head is ≥ the page count
- the free head and free count disagree about whether the list is empty
- the free count is ≥ the page count

A non-empty file shorter than one page is `BadMagic` if it doesn't start with the
magic (it was never a database, e.g. a text file), and `Corrupt` if it does.
The file must be at least `page count × 4096` bytes. A **longer** file is
accepted and its tail ignored (a flush can stop after writing pages but before
updating the header). A **shorter** file has lost pages and is reported as corrupt.

### Free list

Freed pages form a singly linked list threaded through the pages themselves:

| Bytes | Field |
| --- | --- |
| 0–4 | id of the next free page (`0` = end of list) |
| 4–4096 | zero |

`free(id)` pushes onto the head and `allocate()` pops from it, so the most
recently freed page is reused first (LIFO). An allocated page always comes back
zeroed. When the list is empty, `allocate()` appends a page and the page count
grows. Page ids are `u32`, so the limit is 2³² − 1 pages (16 TB).

**Decision:** an in-page linked list needs no extra pages and no size limit,
and the header alone records the whole list. The cost is a page read per
allocation from the list, which the cache usually absorbs. Freeing a page twice
corrupts the list. That is the caller's job to avoid, and the B+ tree is the only caller.

## Pager and buffer cache

The `Pager` owns page 0. Callers use pages `1..page_count`, and touching page 0
or anything past the end is `PageOutOfRange`.

**Copy in, copy out.** `read_page` returns an owned copy of the page (`Box<[u8; 4096]>`).
`write_page` copies a whole page back into the cache and marks it dirty.
Callers never hold a borrow into the cache, so the B+ tree can hold several
pages at once (needed for splits) with no pinning or lifetimes.
**Trade-off:** a 4 KB copy per access. This will be revisited if benchmarks show it matters.

**Cache.** A fixed number of frames (`DEFAULT_CACHE_FRAMES` = 256, i.e. 1 MB) with
least-recently-used (LRU) eviction:

- Frames live in fixed slots. A `HashMap` maps each page id to its slot.
- The LRU order is a doubly linked list threaded through the slots by index
  (`prev` and `next` arrays). Head = most recently used, tail = next to evict.
  Hit, insert and evict are all O(1) with no allocation.
- **On a miss:** evict the tail if the cache is full. A dirty victim is written
  back first, and if that write fails the victim stays cached so the change isn't lost.
  The new page is then read into the victim's reused buffer. `write_page` skips
  the read, since it replaces the whole page.

**Flush** writes the dirty pages in ascending page order, then the header (if
changed), then calls `sync`. Pages go before the header so the header never
counts pages that aren't in the file yet. `close()` flushes. Dropping a pager
without `close()` discards unflushed changes on purpose: hidden writes would get
in the way of the crash tests. Real crash safety comes from the write-ahead log.

## Tracer

Every layer reports events to a shared `Arc<dyn Tracer>`. The trait's one
method has an empty default body. `NoopTracer` is the normal setting, and
`RecordingTracer` collects events for tests and, later, `.trace on` and the visualizer.

Pager events so far:

| Event | When |
| --- | --- |
| `CacheHit { page }` | A requested page was cached |
| `CacheMiss { page }` | A requested page was not cached |
| `CacheEvict { page, dirty }` | A frame was freed to make room |
| `PageRead { page }` | Bytes were read from storage (including the header on open) |
| `PageWrite { page }` | Bytes were written to storage (evicted page, flush, header) |

On a miss the order is always: `CacheMiss`, then `CacheEvict`, then (if dirty)
the victim's `PageWrite`, then `PageRead`. The visualizer can replay that order as it stands.

**Decision:** use a trait object, not a generic parameter. With tracing off,
each event costs one call to an empty function, which is tiny next to any I/O.
A generic `Tracer` parameter would cost nothing at all, but every type above
the pager would have to carry it (`BTree<T>`, `Engine<T>`, …).

B+ tree events (the tree reaches the shared tracer through `pager.tracer()`):

| Event | When |
| --- | --- |
| `NodeVisit { page }` | A node is read on the way to a key (sent before the page read) |
| `NodeSplit { page, new_page, separator }` | A full node split; `separator` went to the parent |
| `NodeMerge { left, right }` | An underfull node and its neighbour merged into `left`; `right` was freed |
| `NodeBorrow { left, right }` | Two neighbours too full to merge shared their entries evenly |
| `RootGrow { root, left, right }` | The root split; its halves moved to `left` and `right` |
| `RootShrink { root, child }` | The root's only child was pulled up into the root page and freed |

## B+ tree

A B+ tree maps byte-string keys to byte-string values, ordered byte by byte
(the same order as `BTreeMap<Vec<u8>, Vec<u8>>`). Every entry lives in a leaf.
Internal nodes hold only separator keys that steer a search, and leaves are
linked left to right for range scans. Tables, indexes and the catalog will all
be B+ trees. Numbers will be stored in a byte form that sorts correctly.

### API

```rust
let tree = BTree::create(&mut pager)?;   // or BTree::open(root_page)
tree.insert(&mut pager, key, value)?;    // -> Option<old value>
tree.get(&mut pager, key)?;              // -> Option<value>
tree.delete(&mut pager, key)?;           // -> Option<old value>
tree.range(&mut pager, Bound::Included(a), Bound::Excluded(b))?;  // iterator, in key order
tree.check(&mut pager)?;                 // verify every invariant -> TreeStats
tree.dump(&mut pager)?;                  // the tree as indented text
```

**Decision:** a `BTree` is just its root page id, and the shared `Pager` is
passed into each call, so any number of trees share one pager and one cache.

**Decision: the root page id never changes.**
- When the root splits, its contents move to two new pages and the root page
  becomes their parent (`RootGrow`).
- When the root is left with one child, that child is copied into the root
  page and freed (`RootShrink`).

So the catalog stores a table's root page once and never updates it.

### Size limits

| Limit | Value | Why |
| --- | --- | --- |
| `MAX_KEY` | 512 bytes | An internal node always holds at least 7 keys |
| `MAX_ENTRY` (key + value) | 1000 bytes | A leaf always holds at least 4 entries |
| `MIN_FILL` | 1024 bytes (¼ page) | Below this, a node other than the root is underfull |

Larger entries are rejected with `EntryTooLarge`. Storing big values across
several pages ("overflow pages") is left for later.

### Node page layout (slotted page)

| Bytes | Field |
| --- | --- |
| 0 | kind: `1` = leaf, `2` = internal |
| 1–3 | cell count (u16) |
| 3–5 | offset where the cell area starts (u16) |
| 5–9 | leaf: next leaf page (`0` = last); internal: rightmost child |
| 9–12 | reserved, zero |
| 12– | slot array: one u16 cell offset per cell, in key order |
| …end | cells, packed backwards from the end of the page |

| Cell | Layout |
| --- | --- |
| Leaf | `key_len u16 · value_len u16 · key · value` |
| Internal | `child u32 · key_len u16 · key`, where `child` holds keys **below** `key` |

An internal node with `n` keys has `n + 1` children. Child `i` holds keys in
`keys[i-1] ≤ k < keys[i]`, and the rightmost child (kept in the header) holds
everything at or above the last key. So a search goes to child number
"how many keys are ≤ the search key".

**Decision: change a decoded copy.** A node is decoded into a plain
`Node { entries / keys + children }`, changed with ordinary `Vec` operations,
and encoded back packed tight. Free space never fragments, and split and merge
logic needs no byte juggling. The cost is decoding a whole node per visit. The
format is still a real slotted page, so a later version can binary-search the
raw page without decoding it, with no format change.

Decoding checks every offset and length, so a damaged page gives `Corrupt`
naming the page instead of a panic.

### Insert and delete: one recursive walk

`modify(page, key, change)` handles both insert and delete. It changes its
subtree, writes its own node back, and reports to the parent:

| Outcome | Meaning | Parent's repair |
| --- | --- | --- |
| `Fine` | nothing to do | none; the parent isn't rewritten |
| `Split { separator, right }` | the node overflowed and split | insert `separator` and `right` after this child |
| `Underfull` | below `MIN_FILL` | merge with or borrow from a neighbour |

A repair changes the parent, which can then overflow or underflow in turn, so
repairs ripple up level by level. The root's outcome is handled by
`RootGrow` / `RootShrink`.

**Splitting (overflow).** Entries vary in size, so the split point is chosen
by **bytes**: the two halves end up as close to equal as possible.
- **Leaf split:** the right half's first key is *copied* up as the separator,
  and the leaf chain is re-linked as left → right → old next.
- **Internal split:** the middle key *moves* up and leaves this level.

A node overflows by at most one cell (one new entry, or a longer separator
after a borrow), so one split always leaves two halves that fit.

**Underflow.** An underfull child is paired with its left neighbour, or with
its right neighbour if it is the first child.
- **Merge:** if the two fit in one page, everything moves into the left page,
  the right page goes back to the free list, and the separator between them is
  removed from the parent. For internal nodes, that separator comes down
  between the two key lists.
- **Borrow:** if they don't fit, the combined entries are split evenly back
  into the same two pages, and the parent gets the new separator.

  Why that leaves both halves full enough: the combined size is over 4 KB and
  a cell is at most ~1 KB, so each half gets at least ~1.5 KB, which is above `MIN_FILL`.

Deleting a key never touches separators above it. A separator only has to
steer searches correctly, not equal a live key.

### Range scans

`range(start, end)` searches down once to the leaf where `start` would be,
skips the entries below `start`, then follows the `next` links. It is an
`Iterator<Item = Result<(key, value)>>`, and it borrows the pager mutably, so
the tree cannot change during a scan.

### Testing

- `btree/node.rs`: encode/decode round trips, the exact byte layout, size
  accounting, and corrupt pages.
- `btree/tests.rs`: one scenario per shape change (first split and root grow,
  internal split, merge and root shrink, borrow, delete-all with no leaked
  pages, range bounds, size limits, empty key, dump). Each checks the exact
  structural events.
- `tests/btree_props.rs` (proptest), compared against `BTreeMap`:
  - 256 random sequences of up to 2,000 inserts, deletes, gets and range scans,
    with mixed short and long keys.
  - 100,000 random inserts and deletes per seed (4 seeds) on a 32-frame cache.
    `check()` runs every 5,000 operations. At the end the tree is emptied and
    must leak no pages.
- **Guarding the tests:** each of these bugs was planted once and made the
  property test fail with a shrunk, minimal example:
  - searching left on a key equal to a separator
  - never rebalancing underfull nodes
  - treating an exclusive range end as inclusive

## Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:
`cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`,
then `cargo test --workspace`.

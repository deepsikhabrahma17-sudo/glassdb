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

## Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:
`cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`,
then `cargo test --workspace`.

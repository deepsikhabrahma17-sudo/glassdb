# GlassDB — Design

This file records the on-disk format and the design decisions behind it.
It grows with each build phase.

## Workspace layout

| Crate | Package name | Kind | Purpose |
| --- | --- | --- | --- |
| `crates/core` | `glassdb-core` | library | The engine: pager, B+ tree, WAL, indexes, planner, SQL, tracer |
| `crates/cli` | `glassdb` | binary | The `glassdb` SQL prompt (`cargo install glassdb`) |

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
- It opens the file and its log, or creates a new database (header page and
  empty catalog). Then it prints a banner with the version, the file name and the
  table count. A file that isn't a GlassDB database is reported and left
  untouched (exit code 1). So is a file that something else has open.
- **Input** collects lines until the text ends with a `;` that isn't inside a
  `'string'` or a `-- comment`. A statement can span lines (the prompt shows
  `   ...>`), and one line can hold several statements. A syntax error in one
  of them runs none; a runtime error stops at that statement.
- **Output:**
  - `SELECT` results print as an aligned table, with numbers right-aligned,
    ending in `(n rows)`.
  - Other statements print a line, such as `inserted 2 rows`.
  - Errors go to stderr as `error: …`, and the session carries on.
- **Dot commands** (only at the start of a statement): `.tables`,
  `.schema [table]`, `.check`, `.checkpoint`, `.help`, `.exit` / `.quit`.
- **After a crash**, the banner says how many transactions were recovered from the log.
- **Transactions:** the prompt is `glassdb*>` while one is open. Leaving with one
  open rolls it back and prints `open transaction rolled back`.
- **Keys:** Ctrl-C drops a half-typed statement. Ctrl-D (end of input) runs
  anything left unfinished, then exits with code 0. So `glassdb my.db < script.sql` works.
- **Code layout:** `repl.rs` (statement buffering) and `render.rs` (tables)
  are pure functions with unit tests. `main.rs` only does I/O.
- `rustyline` provides line editing and history on a terminal, and plain line
  reads when stdin is piped. The integration tests depend on that piped mode.

## Storage

The pager and the log read and write through a `Storage` trait (`read_at`,
`write_at`, `truncate`, `size`, `sync`), not through `File` directly.

- `FileStorage` is a real file, used by the CLI. It can also take the file's
  exclusive lock (see [The lock](#the-lock)).
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
accepted and its tail ignored. A **shorter** file has lost pages and is reported as
corrupt. Both checks run after recovery, so pages still in the log don't count as lost.

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

A pager opens in one of two ways. `open_with_wal` is crash safe and is what
`Database` uses. `open` has no log and writes straight to the main file; the
lower-level tests and tours use it because it's simpler and faster. This section
describes what both share, and the Write-ahead log section covers the rest.

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

**Flush without a log** writes the dirty pages in ascending page order, then the
header (if changed), then calls `sync`. Pages go before the header so the header
never counts pages that aren't in the file yet.

**Dropping a pager** without `close()` discards everything since the last flush,
just as a crash would. There are no hidden writes, which is what lets tests
simulate a crash by dropping one.

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

Log events:

| Event | When |
| --- | --- |
| `WalAppend { page }` | A changed page was appended to the log |
| `WalRead { page }` | A page was read from the log, because the main file's copy is out of date |
| `WalCommit { frames }` | A commit record reached disk: the transaction now survives any crash |
| `Checkpoint { pages }` | The log's pages were copied into the main file and the log was emptied |
| `WalRecovered { transactions, frames, discarded }` | Opening found a log left by a crash |

With a log, an evicted dirty page sends `WalAppend` instead of `PageWrite`, and a
page found in the log sends `WalRead` instead of `PageRead`.

Transaction events:

| Event | When |
| --- | --- |
| `TxnBegin` | `BEGIN` ran |
| `TxnCommit` | `COMMIT` finished, after its `WalCommit` |
| `TxnRollback { frames }` | Uncommitted changes were thrown away; `frames` had already spilled to the log |

A statement outside a transaction sends no `TxnBegin` or `TxnCommit`. It sends
`TxnRollback` only if it fails part-way.

`Plan { steps }` reports what a `SELECT`, `UPDATE` or `DELETE` is about to run,
with the same lines `EXPLAIN` prints.

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

## Write-ahead log

**The rule:** a transaction counts only once its commit record is on disk. A
statement outside `BEGIN`/`COMMIT` is a transaction of its own. Anything after
the last valid commit record is thrown away on recovery, so after a crash a
transaction is either wholly there or not there at all.

`my.db` keeps its log in `my.db-wal`, beside it. The log file exists while the
database is open, and after a crash. A clean close removes it.

### The write path

1. A statement changes pages in the cache.
2. If the cache fills, a changed page is appended to the log as an uncommitted
   frame, and can be read back from there. It never goes to the main file.
3. When the statement finishes, `Pager::flush` appends the remaining changed
   pages, then the header page if it changed.
4. A commit record is appended and the log is forced to disk with `sync`. Only
   now does the statement count as done.
5. The main file is still untouched. A later checkpoint updates it.

A read looks in the cache, then the log, then the main file. In the log, the
running transaction's frames are checked first, then committed frames, and the
newest frame of a page wins.

**Decision: the main file changes only during a checkpoint.** The alternative is
writing each commit into the main file straight away and keeping the log only
for redo. That needs no log lookup on reads. But then a statement bigger than the
cache has nowhere to put its pages before it commits, and it would make `ROLLBACK`
harder. Here, an unfinished statement only ever exists in the log, so undoing it
just means forgetting those frames.

**The header page is logged like any other page**, so page count and free-list
changes commit together with the data. A new database is created the same way:
nothing reaches the main file until the first checkpoint. A crash during creation
therefore leaves an empty file, never a broken one.

### File format

All integers are little-endian. The file starts with a 16-byte header:

| Bytes | Field |
| --- | --- |
| 0–8 | magic `GLASSWAL` |
| 8–12 | format version (`1`) |
| 12–16 | zero |

Then records, one after another:

| Bytes | Field |
| --- | --- |
| 0 | kind: `1` = page frame, `2` = commit |
| 1–4 | zero |
| 4–8 | page frame: page id; commit: number of frames in this transaction |
| 8–12 | checksum |
| 12– | page frame only: the 4096-byte page |

**Decision: checksums are chained.** Each record's CRC32 covers the previous
record's checksum as well as its own bytes (the first starts from 0). A record
is therefore only valid in its exact place in the log, so a torn, damaged or
out-of-order record fails the check. It also means the commit record vouches for
the whole transaction: a bad frame makes the commit's checksum fail too.

### Recovery

On open, the log is scanned from the start. Frames are held back until a valid
commit record arrives, and the scan stops at the first record that is incomplete
or fails its checksum. Every committed page is copied into the main file, the
main file is synced, and the log is emptied. Frames after the last commit are
thrown away. The `WalRecovered` event and `Database::recovered()` report the result.

A log with a damaged header can't hold anything committed, because a commit is
only synced after the header. So a bad magic, or a version of 0 from a header torn
part-way, counts as an empty log. A valid header with a newer version is refused.

### Checkpoint

`copy_log_to_main` copies the newest committed version of each page into the main
file, syncs the main file, then empties the log (truncate, then sync).

**The order is what makes it safe.** The log is emptied only after the main file
is synced. If a crash lands anywhere in the checkpoint, the log is still complete
and the next open simply copies the pages again. Copying a page twice is harmless.

A checkpoint happens on open (as part of recovery), on clean close, when a commit
leaves the log with 1,000 or more frames (`CHECKPOINT_FRAMES`, about 4 MB), and on
demand with `.checkpoint`.

### Other details

- **Folder sync:** `Database::open` syncs the database's folder after creating
  the log file. Otherwise a crash could lose the file's name and everything
  committed to it. It's best effort, because not every system can sync a folder.
- **Existing databases** open normally: the main file format is unchanged.
- **Verification:** `Database::check` (`.check` in the CLI) verifies every B+ tree,
  decodes every row, walks the free list, and confirms that used pages + free
  pages = pages in the file. So a leaked page counts as corruption.

### Limits

- **A failed `sync`:** if the sync of a commit fails, there's no telling whether
  the commit record reached the disk. The statement reports an error and is
  rolled back, but a crash straight after could still bring it back.
- **A failed automatic checkpoint** doesn't fail the commit that triggered it,
  because that commit is already safe. The log stays whole, and the next commit
  tries again.
- **Speed:** every commit waits for a `sync`, so writes are limited by the disk.
  A transaction turns many statements into one commit.

### Crash testing

- **Unit tests** (`wal/tests.rs`, `pager/wal_tests.rs`): the exact byte layout, a
  log cut at every byte position, a header torn at every byte, flipped bits,
  swapped frames, and spilled pages.
- **Simulated power cuts** (`crates/core/tests/crash_sim.rs`): killing a process
  can't show what a power cut does, because the operating system still finishes
  every write it was handed. This test's fake disk does. Anything written since
  the last sync may survive whole, survive torn, or be lost, chosen at random
  for each write. An 86-step workload, transactions and indexes included, runs and
  the power is cut at every one of its 689 disk operations, with 3 seeds. Sometimes the
  power fails again during recovery. After each cut the database must open, pass
  `check`, and hold exactly the state after the last confirmed step, or after the
  one in flight.
- **Real kills, the done-when check** (`crates/cli/tests/crash.rs`): the real
  `glassdb` binary runs a stream of statements with `.checkpoint` mixed in and
  gets SIGKILL at a random moment, 200 times, on one growing database. After every
  kill: `check` must pass, every confirmed statement must be there, every INSERT
  batch must be whole, and an UPDATE of all rows must have reached all of them.
  [Transactions](#transactions) adds its own checks to both tests.
- **Guarding the tests:** each of these bugs was planted once and made a test fail:
  - commit doesn't sync the log
  - recovery ignores checksums
  - uncommitted frames count as committed
  - a checkpoint empties the log before the main file is synced
  - a dirty page evicted straight into the main file

  Removing only the frame's checksum check caught nothing, which is expected: the
  chained commit checksum still rejects the transaction.

The power-cut test found a real bug while it was being built. The log header can
tear between the magic and the version, and recovery read that as "unsupported
version 0" and refused to open the database.

## Transactions

```sql
BEGIN [TRANSACTION];
UPDATE accounts SET balance = balance - 10 WHERE id = 1;
UPDATE accounts SET balance = balance + 10 WHERE id = 2;
COMMIT;      -- or ROLLBACK
```

Outside a transaction, each statement commits when it finishes. Inside one,
nothing commits until `COMMIT`, and a transaction sees its own changes. Nothing
changed on disk to support this: the file formats are the same as before.

### Commit and rollback

`COMMIT` is the commit a single statement already did: `Pager::flush` appends the
changed pages and a commit record, then syncs. A transaction's changes live in the
same three places as a statement's: the cache, uncommitted frames in the log, and
the in-memory copy of the header.

`ROLLBACK` undoes all three:

1. **The cache is emptied, all of it.** Dropping only dirty frames would be a bug.
   A page that spilled to the log and was read back is clean in the cache, yet
   still holds uncommitted bytes.
2. **The header goes back to the last committed copy**, which undoes page
   allocations and frees. `Pager` keeps that copy, updated on every commit.
3. **The log is cut back to the last commit.** `Wal` keeps the end position and
   checksum as of that commit, so the next record chains from there.

**Decision: a rollback writes nothing and needs no sync.** It leaves no record in
the log. If a crash undoes the cut and brings frames back, they still have no
commit record, so recovery throws them away. This is what makes "no trace, even
after a crash" true, and it comes from Phase 4's rule rather than from new code.

The main file is never involved, because it only changes during a checkpoint.

**The catalog needs no special care.** It's a B+ tree with no state in memory, so
`CREATE TABLE` and `DROP TABLE` roll back like any other page change. A table's
pages return to wherever they were, and `check` still accounts for every page.

### When a statement fails

**Decision: an error found before writing leaves the transaction open.** Statements
check everything before they write, so a duplicate key or a wrong type has changed
nothing. The transaction carries on, as in SQLite. PostgreSQL aborts the whole
transaction instead, which would throw away work for no reason here.

Any other error (a failed disk write, corruption) may strike with a statement half
done. So:

- **Inside a transaction,** it moves to a failed state. Every statement, `COMMIT`
  included, is refused until `ROLLBACK`. Rolling back at once would be worse,
  because the statements typed after it would each commit on their own, and the
  closing `COMMIT` would then be the first sign of trouble.
- **Outside a transaction,** the statement is rolled back straight away.
  Otherwise the next statement's commit would sweep up its half-written pages.

### Rules

- `BEGIN` inside a transaction, and `COMMIT` or `ROLLBACK` outside one, are errors.
- `.checkpoint` inside a transaction is refused, because a checkpoint empties the
  log and would lose its frames. Automatic checkpoints only run just after a
  commit, when no transaction is running.
- `Database::close` rolls back an open transaction. Quitting isn't a promise to commit.
- A transaction can be bigger than the cache and spills to the log. The log has no
  size limit while one runs, since `CHECKPOINT_FRAMES` is only checked at a commit.
  Rolled-back frames don't count towards it.
- `BEGIN`, `COMMIT`, `ROLLBACK` and `TRANSACTION` are not reserved words. They mean
  something only at the start of a statement, where a name can't appear. The
  catalog parses its stored `CREATE TABLE` text on open, so reserving them would
  stop any database with a table or column of those names from opening.

### The lock

**Decision: one open at a time.** `Database::open` takes an exclusive lock on the
database file, or fails at once with `Error::Locked`. It's held until the
`Database` is closed or dropped. Readers are refused too.

This is stricter than the "one writer at a time" a database needs in the end, and
it's needed anyway. Opening a database checkpoints and empties its log, so a second
open would destroy a running writer's log. Sharing a file between programs safely
would take a shared index of the log, like SQLite's `-shm` file, and readers that
stay stable while a writer runs. Phase 7's row versions are the place to revisit that.

Within one process, `Database::run` takes `&mut self`, so Rust already allows only
one writer. A second `Database` on the same file is refused like any other.

- **How:** `File::try_lock` from the standard library, which is `flock` on Linux and
  macOS. No lock file is left on disk, and the operating system releases the lock
  when the process dies, so a crash can't leave a stale lock. A lock file would.
- **Order on open:** the lock comes before the log is touched, so a refused open
  changes nothing.
- **Order on close:** the log is removed before the lock is released. The other
  way round, a new opener could create its log just in time for this one to delete it.
- **Only real files are locked.** `MemStorage` has nothing to share.

### Testing

- **Unit tests:** `wal/rollback_tests.rs`, `pager/rollback_tests.rs`,
  `exec/txn_tests.rs` and `sql/txn_tests.rs`. A storage wrapper makes the log fail
  on demand, which reaches the failed state.
- **SQL file:** `tests/sql/transactions.test`, with restarts.
- **Lock:** `crates/core/tests/lock.rs`, plus a real second `glassdb` process in
  `crates/cli/tests/cli.rs`.
- **Property test** (`crates/core/tests/txn_props.rs`): random statements,
  transactions, table changes, crashes and restarts on a 4-frame cache, against an
  in-memory model. The two must agree after every step, and `check` must pass
  whenever no transaction is open.
- **Simulated power cuts, the done-when check:** the workload now rolls back
  transactions that spilled, commits shorter ones over the same stretch of the log,
  and rolls back `CREATE TABLE` and `DROP TABLE`. After each of its 2,067 cuts the
  database must hold exactly the last committed state, or the one in flight. The
  rolled-back rows say `ghost`, and that word must never appear.
- **Real kills:** each batch adds a transaction that rolls back and one that commits
  to two tables. After every kill no rolled-back row may be there, the two tables
  must match, and the file must open at once, which shows the lock was released.
- **Guarding the tests:** 21 bugs were planted one at a time in the rollback,
  commit, close and lock code, and each made a test fail. Eight of the worst were
  then run against the crash tests alone:
  - The power-cut test caught seven. It missed `close` committing an open
    transaction, because its workload never closes with one open.
  - The model test caught all eight.
  - The kill test caught three, and missed the bugs that need spilled pages. The
    real binary's 256-page cache is too big for its small batches to spill, so
    that case relies on the 4-frame power-cut and model tests.

## Indexes and the planner

```sql
CREATE INDEX [IF NOT EXISTS] by_age ON users (age, name);
DROP INDEX [IF EXISTS] by_age;
EXPLAIN SELECT * FROM users WHERE age = 30 AND name > 'a';
```

**The rule: an index changes speed, never results.** Whichever way rows are found,
the whole `WHERE` is still applied to every one of them. So a key range may hold
rows that don't match, but must never miss one that does, and the planner can leave
anything it isn't sure about to that filter. Rows found through an index are also
read in table order, so a query gives the same rows in the same order with or
without one. `tests/index_props.rs` tests exactly that.

### Format

An index is a B+ tree of its own with an entry for every row, NULLs included. The
entry's value is empty and its key is the indexed columns then the row id:

| Tag | Type | Bytes after the tag |
| --- | --- | --- |
| 1 | NULL | nothing |
| 2 | BOOL | 1 byte, 0 or 1 |
| 3 | INT | 8 bytes, the same as a table key |
| 4 | REAL | 8 bytes: the IEEE bits with the sign bit set if positive, all flipped if negative |
| 5 | TEXT | the bytes, with 0 written as `0 255`, then the end marker `0 1` |

- **Byte order equals value order**, column by column, so the B+ tree needs no idea
  what a key means. This is property-tested against `sort_cmp`.
- **The row id** makes keys unique, so many rows can share a value, and says which
  row an entry is for.
- **The end marker** keeps `('a', 'z')` before `('ab', 'a')`. Without it the bytes
  of the second column would run into the first. Escaping zeros keeps the marker
  unambiguous and below every other byte.
- `-0.0` is written as `0.0`, because they compare equal.
- **A column has one type**, because `fit` converts values on the way in. So tags
  only have to place NULL first; they never compare two types.

Indexes share the catalog tree with tables, as a root page plus `CREATE INDEX` text,
and the SQL says which kind an entry is. So they also share one set of names, as in
SQLite. Nothing is kept in memory, so `ROLLBACK` undoes them with no extra code. The
file format version didn't change: existing databases open as they are, but a build
from before this phase can't read a database that has indexes.

**Decision: a value too long to index is an error.** B+ tree keys stop at 512 bytes
(`MAX_KEY`), but a row can reach 992. The alternative is storing a prefix, which is
possible because `WHERE` is rechecked anyway, but ranges need much more care. The
error comes during a statement's checking pass, so it changes nothing, and
`CREATE INDEX` works out every key before it writes.

### Keeping indexes in step

`INSERT`, `UPDATE` and `DELETE` compute index keys in their checking pass and write
them after the table. `UPDATE` skips an index whose key didn't change, which avoids
needless writes, and removes every old entry before adding any new one, because one
row's new entry can be another row's old one. `DROP TABLE` drops its indexes.
`CREATE INDEX` sorts its keys first, so it fills the tree left to right and touches
each leaf once.

`Database::check` now also proves each index holds exactly its table's rows: it
works out every key from the rows and compares the two lists. So the power-cut and
kill tests verify indexes after every crash without knowing about them.

### Choosing a plan

`planner::choose` is a pure function of the schema and `WHERE`. It reads no pages.

1. Split `WHERE` at each top-level `AND`. Anything under an `OR` or `NOT` is ignored.
2. Keep conditions of the form `column op constant`, with `=`, `<`, `<=`, `>` or `>=`.
   The constant may be on either side, and may be an expression like `20 + 10`.
3. Combine them per column. `a > 1 AND a > 3 AND a < 9` becomes one range.
4. Score each way of finding rows, and take the best:

| Path | Example | Rank |
| --- | --- | --- |
| `PRIMARY KEY LOOKUP` | `id = 5` | 1 |
| `INDEX SCAN` with `=` on leading columns | `age = 30` | 2 (more `=` columns win, then a range on top) |
| `PRIMARY KEY RANGE` | `id > 5` | 3 |
| `INDEX SCAN` with only a range | `age > 30` | 4 |
| `FULL SCAN` | anything else | 5 |

Ties go to the first index by name, so a plan never depends on creation order.

An index on `(a, b, c)` uses `=` on its leading columns, then one range on the next
column. `a = 1 AND b > 2` uses two columns. `a > 1 AND b = 2` and `a = 1 AND c = 3`
each use one: nothing can follow a range or a gap.

**What the planner refuses,** so the scan gives the right answer:

- **NULL constants.** `a = NULL` matches nothing, and the filter says so.
- **A constant that fails to evaluate,** such as `a = 1 / 0`. On an empty table a scan
  never evaluates it, so failing during planning would be a new error.
- **Mismatched types.** `a = 'x'` on an INT column is an error, and the scan reports it.
- **An INT column against a REAL constant.** INT and REAL compare through `f64`, which
  rounds large integers, so no key bound would be exact. A REAL column with an INT
  constant is fine, because that's the same conversion the comparison makes.
- **NaN.** See below.

### Key ranges

A key is the columns then a row id, so a bound can't name an exact key.
`successor(bytes)` gives the point just past everything starting with those bytes:

| Condition | Start | End |
| --- | --- | --- |
| `a = v` | `v` included | successor of `v`, excluded |
| `a > v` | successor of `v`, included | past the prefix |
| `a >= v` | `v` included | past the prefix |
| `a < v` | first non-NULL value | `v` excluded |
| `a <= v` | first non-NULL value | successor of `v`, excluded |

The prefix is the encoded values of the `=` columns before this one.

### Running a plan

`exec/access.rs` turns a plan into rows. An index scan collects row ids from the
range, sorts them, and fetches each row from the table. Sorting keeps results in
table order, and also reads the table left to right. `SELECT`, `UPDATE` and `DELETE`
all find their rows this way, and the `WHERE` filter comes after.

If an index names a row that isn't there, the query fails with `Corrupt` rather than
quietly returning fewer rows.

### EXPLAIN

`EXPLAIN` checks a statement exactly as running it would, then returns its steps as
rows with one column, `plan`, without running it:

```text
INDEX SCAN by_age ON users (age > 30)
FILTER age > 30
SORT name
LIMIT 10
```

It works on `SELECT`, `UPDATE` and `DELETE`. Keeping the steps as data means the
tracer's `Plan` event and the visualizer can show the same thing.

### NaN

`0 / 0` is an error, but infinity minus infinity gives NaN, and `Value::compare`
treats NaN as equal to everything. A full scan would match it on any `=`, and an
index couldn't, so the two would disagree. Storing NaN in a column is now an error,
and the planner ignores NaN constants. A database that already holds a NaN from an
earlier build keeps working, but only its full scans treat it this way.

### Limits

- **No statistics,** so the planner goes by the query's shape. An index on a
  low-variety column like a `BOOL` can be slower than a scan, since every match is
  fetched by key.
- **`ORDER BY` always sorts.** A query doesn't use an index to avoid that.
- **Every matching row is read in full.** There are no covering indexes.
- **`IS NULL` isn't looked up** in an index, though NULLs are stored in it.
- **No `UNIQUE` indexes** yet.
- **Errors can depend on the plan.** A condition that fails on some rows, such as
  `b / 0 = 1`, only runs on rows that are found. So a query that fails in a full scan
  can succeed with an index. SQL allows this, and it can only remove an error, never
  change a result.

### Testing

- **Unit tests:** key order, each planner rule, and each executor path, in
  `types/index_key_tests.rs`, `planner/tests.rs`, `exec/index_tests.rs` and
  `sql/index_tests.rs`.
- **SQL file:** `tests/sql/indexes.test`, with restarts.
- **The comparison** (`crates/core/tests/index_props.rs`): random statements run on
  two databases, one indexed and one not. Every statement must give identical
  output, through transactions, crashes, and indexes being created and dropped, and
  `check` must pass after each one.
- **Pages read** (`crates/core/tests/index_speed.rs`): on 20,000 rows, a lookup
  through an index reads at most 10 pages, against over 200 for a scan. Counting
  pages can't fail because a machine is busy.
- **Crash tests:** the power-cut and kill workloads now write through indexes, and
  `check` verifies them after every crash.
- **The done-when check** (`crates/core/examples/index_bench.rs`), on 1,000,000 rows
  in a real file, in release mode on the development Mac:

| Lookup | Time |
| --- | --- |
| Full scan (`n = ?`) | 119.68 ms |
| Index (`n = ?`) | 20.84 µs |
| Primary key (`id = ?`) | 13.71 µs |

Inserting took 15.8 s and `CREATE INDEX` took 7.0 s. It's too slow for CI, so run it
by hand with `cargo run --release -p glassdb-core --example index_bench`.

- **Guarding the tests:** 21 bugs were planted one at a time, covering each write
  skipping an index, wrong range bounds, a broken key encoding, misread conditions,
  and rows returned in index order. Each made a test fail. The comparison test alone
  caught 17, and the other four fell to the planner, unit and SQL tests:
  - `DROP TABLE` leaving its indexes, which the comparison never runs
  - an INT column taking a REAL constant, and text zeros left unescaped, which its
    random statements didn't happen to expose
  - `check` not comparing entries, which has no effect until an index is wrong

  A `DELETE` that leaves its index entry was also run against the crash tests alone.
  The power-cut test caught it at once. The kill test missed it, because nothing it
  committed was ever deleted, so it now deletes rows from an indexed table too.

  Moving a range bound outward isn't a bug under the rule above: `WHERE` filters the
  extra rows out. Only a bound that misses rows was planted.

## SQL

### Statements

```sql
CREATE TABLE [IF NOT EXISTS] t (col TYPE [PRIMARY KEY], ...)
DROP TABLE [IF EXISTS] t
INSERT INTO t [(col, ...)] VALUES (expr, ...), ...
SELECT * | expr [[AS] alias], ... [FROM t] [WHERE expr]
       [ORDER BY expr [ASC|DESC], ...] [LIMIT n [OFFSET m]]
UPDATE t SET col = expr, ... [WHERE expr]
DELETE FROM t [WHERE expr]
BEGIN [TRANSACTION]
COMMIT
ROLLBACK
CREATE INDEX [IF NOT EXISTS] i ON t (col, ...)
DROP INDEX [IF EXISTS] i
EXPLAIN SELECT ... | UPDATE ... | DELETE ...
```

- **Types:** `INT` (also spelled `INTEGER`), `REAL`, `TEXT`, `BOOL` (also
  `BOOLEAN`). `NULL` fits any column.
- **Names:** keywords and names are case-insensitive, and names are stored in
  lower case. Type names are ordinary words, not keywords, so a column may be
  called `text`.
- **Not supported yet:** joins, aggregates (`COUNT`, …), `LIKE`, qualified names (`t.col`), double-quoted names.

### Front end: tokens → syntax tree

- **Tokenizer** (`sql/token.rs`):
  - Every token records its line and column, so errors read
    `syntax error at 1:8: expected an expression, found FROM`.
  - `''` inside a string is a quote character.
  - `--` starts a comment.
  - Double quotes get a hint to use single quotes.
- **Parser** (`sql/parser.rs`, `sql/expr.rs`): recursive descent, one method
  per grammar rule. Expressions have one method per precedence level, loosest first:

  | Level | Operators |
  | --- | --- |
  | 1 | `OR` |
  | 2 | `AND` |
  | 3 | `NOT` |
  | 4 | `= <> != < <= > >=`, `IS [NOT] NULL` (comparisons don't chain) |
  | 5 | `+ -` |
  | 6 | `* / %` |
  | 7 | unary `-` |

- **Number literals:**
  - A number with `.` or an exponent is REAL, otherwise INT.
  - A minus written right before a number is part of the literal, so
    `-9223372036854775808` (`i64::MIN`) can be written.
  - `-(5)` is folded into the literal `-5`.
- **Printing back:** every syntax tree prints back as tidy SQL with only the
  parentheses it needs, and parsing the printed text gives the same tree
  (tested). Printed SQL is used in three places:
  - the catalog stores each table as a printed `CREATE TABLE`
  - a `SELECT` column without an alias is named by its printed expression
  - the tracer's `Statement` event carries the printed statement

### Values and rules

- **INT and REAL mix freely.** `INT op INT` gives an INT, and a REAL on either
  side gives a REAL. INT and REAL compare as numbers.
- **Integer errors:** integer overflow is an error, and so is dividing by zero
  (INT or REAL). An error is chosen over a guess.
- **Type mismatches are errors.** Comparing different types (`1 = 'one'`),
  arithmetic on TEXT or BOOL, `NOT 1` and `TRUE AND 1` all fail.
- **NULL means unknown**, so expressions follow three-valued logic:
  - `NULL + 1` and `NULL = NULL` are NULL.
  - `NULL AND FALSE` is false, and `NULL OR TRUE` is true: the deciding value
    wins even against unknown.
  - `WHERE` keeps a row only if the condition is TRUE.
- **Storing values:** a value must match its column's type, except that an
  INT is accepted into a REAL column and stored as REAL. Errors name the
  column: `column name is TEXT, got INT 5`.
- **Sorting:** `ORDER BY` puts NULLs first, or last with `DESC`. The sort is
  stable. `ORDER BY` can name an output alias (`ORDER BY curved`); a real
  column with the same name wins.
- **Limitation:** column names are checked before any row is read, but value
  types only as rows are evaluated. So `SELECT * FROM t WHERE 1` errors only
  if `t` has a row. Checking types before running is left for the planner.

### Rows and keys on disk

Each table is a B+ tree. The **key** is the row id as 8 bytes. The sign bit
is flipped and the bytes are written big-endian, so byte order equals numeric
order (property-tested for all `i64`). The **value** is the row:

| Bytes | Field |
| --- | --- |
| 0–2 | column count (u16) |
| … | for each column: a type tag byte, then its data |

| Tag | Type | Data |
| --- | --- | --- |
| 0 | NULL | none |
| 1 | INT | 8 bytes, little-endian i64 |
| 2 | REAL | 8 bytes, little-endian f64 |
| 3 | TEXT | u16 length + UTF-8 |
| 4 | BOOL | 1 byte |

- Decoding checks every length, and rejects extra bytes and unknown tags.
- A row may be at most `MAX_ENTRY - 8` = 992 bytes (the B+ tree's entry limit
  minus the key). Larger rows are rejected with `row too large`. Overflow
  pages would lift this.

**Decision: `INT PRIMARY KEY` is the row id** (as in SQLite).
- The primary key's value is the B+ tree key, so it is unique by
  construction, and duplicates give `duplicate primary key: t.id = 3`.
- Without a primary key, or when one is given as NULL, a row gets the next id:
  one more than the largest key, found with `BTree::last()`.
- Only one INT column can be the primary key.

### Catalog

- The catalog is the B+ tree whose root is **page 1**. It is created when the
  database is new (only the header exists, so the first allocated page is 1).
  Because a tree's root never moves, page 1 stays the catalog. The file header
  needed no change.
- Each entry maps a table or index name to its root page (4 bytes, little-endian)
  followed by its `CREATE TABLE` or `CREATE INDEX` statement as tidy SQL.
- On every lookup the SQL is parsed again to rebuild the schema, as SQLite
  does. The schema stays human-readable, and `.schema` prints it straight back.
- Opening a file reads page 1 at once, so a file without a valid catalog fails
  to open with `Corrupt`.
- `CREATE TABLE` checks the definition before allocating anything: an
  existing table, duplicate columns, more than one primary key, or a non-INT
  primary key are all rejected.
- `DROP TABLE` frees every page of the table and its indexes with
  `BTree::destroy()`, then removes the catalog entries.

### Executor

`Database` (`exec/`) runs statements:

```rust
let mut db = Database::open("my.db")?;
db.execute("INSERT INTO t VALUES (1, 'x')")?;   // -> Output::Inserted(1)
for statement in sql::parse(script)? { db.run(&statement)?; }
db.close()?;
```

`Output` is `Message` (CREATE/DROP and transaction statements), `Inserted(n)`,
`Updated(n)`, `Deleted(n)`, or `Rows { columns, rows }` (SELECT and EXPLAIN).

- **SELECT:**
  1. Check every column name.
  2. Find the rows the plan says to, in key order.
  3. Filter with WHERE.
  4. Compute the output columns.
  5. Sort (stable).
  6. Apply OFFSET, then LIMIT.

  `SELECT` without `FROM` computes one row. Step 2 is where
  [the planner](#indexes-and-the-planner) comes in, and step 3 always runs.
- **Decision: check everything, then write.**
  - INSERT evaluates and type-checks every row, assigns ids and checks keys
    before writing any of them.
  - UPDATE computes every new row first. A row whose key changes may move onto
    a free key, or onto a key another changed row is leaving, but not onto a
    row that stays. Moved rows are deleted, then every new version is written.
  - SET expressions all see the old row, so `SET a = b, b = a` swaps.

  As a result a statement rejected with an error changes nothing.
- Every statement that changes data commits to the write-ahead log when it
  finishes, or at `COMMIT` inside a transaction. A crash at any moment keeps it
  whole or loses it whole.
- The tracer gets `Statement { sql }` when a statement starts and
  `StatementDone { summary }` when it finishes.

### SQL test files

`tests/sql/*.test` are run by `crates/core/tests/sql_files.rs` against a real
database file in a temporary directory.

```text
statement ok              # must succeed
CREATE TABLE t (a INT)

statement error no such   # must fail with a message containing the text
SELECT * FROM missing

query                     # output must match the lines after ----
SELECT a FROM t
----
a

restart                   # close and reopen from the file on disk
```

`basics.test` is Phase 3's done-when check. It runs 54 statements covering
every statement kind, with two `restart`s, and repeats the same queries after
each restart. The harness fails if it ever drops below 50 statements.
`expressions.test` covers operators and NULL logic. `errors.test` checks
error messages, and that failed statements write nothing. `transactions.test`
covers `BEGIN`, `COMMIT` and `ROLLBACK` across restarts. `indexes.test` covers
indexes and `EXPLAIN`.

## Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:
`cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`,
then `cargo test --workspace`.

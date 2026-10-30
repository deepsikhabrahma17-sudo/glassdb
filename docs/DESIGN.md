# GlassDB — Design

This file records the on-disk format and the design decisions behind it.
It grows with each build phase.

## Workspace layout

| Crate | Package name | Kind | Purpose |
| --- | --- | --- | --- |
| `crates/core` | `glassdb-core` | library | The engine: pager, B+ tree, WAL, MVCC, SQL, planner, tracer |
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
- It opens the file, or creates a new database (header page and empty
  catalog). Then it prints a banner with the version, the file name and the
  table count. A file that isn't a GlassDB database is reported and left
  untouched (exit code 1).
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
  `.schema [table]`, `.help`, `.exit` / `.quit`.
- **Keys:** Ctrl-C drops a half-typed statement. Ctrl-D (end of input) runs
  anything left unfinished, then exits with code 0. So `glassdb my.db < script.sql` works.
- **Code layout:** `repl.rs` (statement buffering) and `render.rs` (tables)
  are pure functions with unit tests. `main.rs` only does I/O.
- `rustyline` provides line editing and history on a terminal, and plain line
  reads when stdin is piped. The integration tests depend on that piped mode.

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
```

- **Types:** `INT` (also spelled `INTEGER`), `REAL`, `TEXT`, `BOOL` (also
  `BOOLEAN`). `NULL` fits any column.
- **Names:** keywords and names are case-insensitive, and names are stored in
  lower case. Type names are ordinary words, not keywords, so a column may be
  called `text`.
- **Not supported yet:** joins, aggregates (`COUNT`, …), `LIKE`, indexes,
  qualified names (`t.col`), double-quoted names.

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
- Each entry maps the table name to the table's root page (4 bytes,
  little-endian) followed by its `CREATE TABLE` statement as tidy SQL.
- On every lookup the SQL is parsed again to rebuild the schema, as SQLite
  does. The schema stays human-readable, and `.schema` prints it straight back.
- Opening a file reads page 1 at once, so a file without a valid catalog fails
  to open with `Corrupt`.
- `CREATE TABLE` checks the definition before allocating anything: an
  existing table, duplicate columns, more than one primary key, or a non-INT
  primary key are all rejected.
- `DROP TABLE` frees every page of the table with `BTree::destroy()`, then
  removes the catalog entry.

### Executor

`Database` (`exec/`) runs statements:

```rust
let mut db = Database::open("my.db")?;
db.execute("INSERT INTO t VALUES (1, 'x')")?;   // -> Output::Inserted(1)
for statement in sql::parse(script)? { db.run(&statement)?; }
db.close()?;
```

`Output` is `Message` (CREATE/DROP), `Inserted(n)`, `Updated(n)`, `Deleted(n)`,
or `Rows { columns, rows }`.

- **SELECT:**
  1. Check every column name.
  2. Scan the whole table in key order.
  3. Filter with WHERE.
  4. Compute the output columns.
  5. Sort (stable).
  6. Apply OFFSET, then LIMIT.

  `SELECT` without `FROM` computes one row. Every scan reads the whole table
  for now; using indexes to read less comes with the query planner.
- **Decision: check everything, then write.**
  - INSERT evaluates and type-checks every row, assigns ids and checks keys
    before writing any of them.
  - UPDATE computes every new row first. A row whose key changes may move onto
    a free key, or onto a key another changed row is leaving, but not onto a
    row that stays. Moved rows are deleted, then every new version is written.
  - SET expressions all see the old row, so `SET a = b, b = a` swaps.

  As a result a statement that fails changes nothing, unless the disk itself
  fails mid-write. That last case needs the WAL and transactions.
- Every statement that changes data is flushed to disk when it finishes, so
  closing the prompt or killing it between statements loses nothing.
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
error messages, and that failed statements write nothing.

## Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:
`cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`,
then `cargo test --workspace`.

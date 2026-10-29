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

At this stage the prompt echoes input and does not open or create the file.
The pager will create the database file when it arrives.

## Continuous integration

GitHub Actions (`.github/workflows/ci.yml`) runs on every push and pull request:
`cargo fmt --check`, then `cargo clippy --workspace --all-targets -- -D warnings`,
then `cargo test --workspace`.

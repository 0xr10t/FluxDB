# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Commands

```bash
cargo build                          # build all crates
cargo test                           # run all tests
cargo test -p storage                # run tests for one crate
cargo test buffer_pool::tests        # run a single test module
cargo clippy -- -D warnings          # lint
```

## Workspace layout

Cargo workspace with six crates under `crates/`:

| Crate | Purpose |
|---|---|
| `common` | Centralized error types (`thiserror`), constants, and the `Key`/`Value` type-system traits |
| `storage` | Disk manager, sharded buffer pool, B+Tree index (MVCC + Lehman-Yao), WAL |
| `db-core` | `TransactionManager` — the MVCC coordinator; `Transaction` and `Snapshot` types |
| `concurrency` | Lock manager (stub — `// TODO`) |
| `parser` | SQL parser (stub) |
| `schema` | Schema definitions (stub) |
| `cli` | `clap`-based CLI frontend; only `Select` is wired up so far |

## Architecture

### Data flow

```
CLI (clap)
  └─▶ BTreeIndex<K,V>          (storage::index)
        ├─▶ BufferPoolManager   (storage::buffer_pool)
        │     ├─▶ BufferPoolShard × 8  (Clock replacement per shard)
        │     └─▶ DiskManager          (raw page I/O, fsync)
        ├─▶ page::{LeafPage, InternalPage}  (4 KB, little-endian binary)
        └─▶ TransactionManager  (db-core)
              ├─▶ CLOG   (HashMap<txn_id, status>)
              └─▶ Snapshot (xmin / xmax / active set)
```

### MVCC (PostgreSQL-style)

Every tuple on a leaf page carries `xmin` (creator txn ID) and `xmax` (deleter txn ID). Visibility is determined at read time by comparing those IDs against the reader's `Snapshot`. `TransactionManager::begin()` acquires a write-lock on `active_txns` **before** fetching the next ID — this strict lock order is required to prevent snapshots from missing in-flight transactions.

Conflict model is **first-writer-wins**: if two transactions try to write the same key, the second gets `IndexError::WriteConflict` (abort immediately) or `IndexError::WaitFor(txn_id)` (drop latch, wait for the blocker to settle, then retry).

### B+Tree concurrency (Lehman-Yao)

Traversal uses shared (read) latches only. An exclusive latch on the leaf is taken only for mutations. Splits propagate upward through a `BTStack` of ancestor page IDs collected during descent.

### Buffer pool sharding

`page_id & SHARD_MASK` selects one of the 8 `BufferPoolShard`s, each with its own `ClockReplacer`. Maximum pool size is 80 frames total (`MAX_FRAMES`). Page size is fixed at 4 KB (`PAGE_SIZE`).

### WAL record layout

```
| LSN (8) | type (1) | key_len (8) | value_len (8) | timestamp (8) | key | value | CRC32 (4) |
```

`Wal::append` writes to a `BufWriter`; `Wal::flush` calls `fsync` on both the file and its parent directory. `WalIterator` validates each record's CRC32 on read.

### Error handling

All error enums live in `common::error` and are re-exported from `common`. Use `thiserror` for new variants. The `#[from]` attribute wires lower-level errors up automatically — do not add manual `From` impls.

### Key / Value trait system

`common::types` defines `Key: Value` with GAT-based borrowed serialization (`SelfType<'a>`, `AsBytes<'a>`). Implement these traits for any new column type. `TypeName` distinguishes internal (built-in) types from user-defined ones via a classification byte prefix.

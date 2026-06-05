# FluxDB — Durability, Recovery, WAL & Vacuum Design

**Standalone design specification.** This single document is the complete design
for FluxDB's durability subsystem: the write-ahead log (WAL), crash recovery,
checkpointing, and vacuum / space reclamation. It is self-contained — everything
needed to understand and implement the subsystem is here. Code-path references
(e.g. `index.rs`, `write_frame_to_disk`) point into the existing FluxDB codebase
so implementers know where each piece lives.

> **Engine stance (read this first).** FluxDB is a **PostgreSQL-inspired,
> index-organized** store: the B+Tree leaf *is* the table (key → value, with
> `xmin`/`xmax` per tuple version on the leaf). Concurrency in the tree is
> **Lehman-Yao** (right-links + high keys; descend-only, one latch at a time).
> Recovery is **redo-only — there is no undo.** Transaction abort is a CLOG
> status flip (no physical or logical rollback); aborted/dead versions are
> reclaimed later by **vacuum**. Structure modifications (splits, page deletion)
> are **not transactional** — they survive whether the txn commits or aborts.
> There are **no CLRs and no undo pass.**

### Contents

**Part I — Architecture & model**
1. Recovery model: Postgres-style, redo-only, no undo
2. Crate layering (why a new `engine` crate)
3. Locked design decisions
4. Cross-cutting invariants

**Part II — Write-ahead log**
5. Why the record format changed
6. The record set, derived from mutation sites
7. Record types
8. Wire format — generic block-reference framing
9. Per-type payloads
10. LSN allocation, `FlushedLSN` & WAL-before-page
11. On-open / corruption handling (torn tail)

**Part III — Recovery**
12. Ownership & startup sequence
13. The redo pass
14. CLOG reconstruction
15. Checkpoints
16. Incomplete SMOs (splits / page deletion)
17. Page-allocation recovery & hole pages
18. What recovery must NOT do

**Part IV — Vacuum & space reclamation**
19. Strategy: prevent / tolerate / reclaim
20. What FluxDB vacuum does NOT need — and why
21. What exists today
22. Reclaim A — in-page compaction
23. Reclaim B — empty-page deletion + delayed recycle
24. Reclaim C — file truncation
25. Prevent — pre-split "bottom-up" deletion
26. WAL / redo invariants for vacuum & split
27. CLOG truncation & the ordering invariant
28. Triggering & throttling (autovacuum)
29. Skip-scan optimization
30. Vacuum concurrency

**Part V — Roadmap & reference**
31. Concurrency & performance — deferred WAL optimizations
32. What has to be built (implementation order)
33. Open items to settle during implementation
34. Tests to add
35. PostgreSQL reference mapping

---
---

# Part I — Architecture & model

## 1. Recovery model: Postgres-style, redo-only, no undo

The recovery algorithm in textbooks (ARIES) has a **redo** pass and an **undo**
pass with CLRs. FluxDB follows **PostgreSQL**, which keeps ARIES's redo half and
**discards the undo half entirely**:

| Concern | Textbook ARIES | FluxDB (Postgres-style) |
|---|---|---|
| Redo | repeat history, pageLSN-gated, FPI | **same** |
| Undo of losers | undo pass + CLRs | **none** — not done |
| Transaction abort | physical undo | **CLOG status flip** (in-memory + WAL `Abort`) |
| Reclaiming aborted/dead rows | undo | **vacuum** (MVCC `xmin`/`xmax` + `is_vacuumable`) |
| Bounding replay | checkpoints | **same** |

So "undo" is replaced by **MVCC + CLOG + vacuum + checkpoints**. A crash leaves
uncommitted changes physically present after redo; they're invisible (their txn
has no `Commit` in the rebuilt CLOG) and reclaimed later by vacuum. There are
**no CLRs and no undo pass.**

Recovery replays history forward from the last checkpoint, gated by per-page
LSNs, repairing torn pages from full-page images. Uncommitted work that survived
into the data files is left in place — MVCC + the rebuilt CLOG make it invisible,
and vacuum reclaims it.

Choosing the no-undo model is also what makes **vacuum a mandatory, first-class
subsystem** rather than an optional one (Part IV).

---

## 2. Crate layering (why a new `engine` crate)

`storage` already depends on `db-core`, so `db-core` **cannot** hold an
`Arc<Wal>` (dependency cycle), and there is no layer above both today (`cli` is a
stub). Recovery, checkpoint scheduling, and the shared `Wal` need a single owner:

```
            ┌─────────────────────────────────────────────┐
            │  engine  (NEW)                               │
            │  • owns Wal (log manager)                    │
            │  • runs recovery in its constructor          │
            │  • schedules checkpoints + vacuum            │
            │  • holds Arc<Wal> shared down into the pool   │
            └───────────────┬───────────────┬─────────────┘
                            │               │
                   ┌────────▼──────┐  ┌─────▼───────────┐
                   │   storage     │─▶│   db-core       │
                   │ buffer pool,  │  │ TransactionMgr, │
                   │ B+Tree, pages │  │ CLOG, MVCC      │
                   └───────────────┘  └─────────────────┘
```

- **`engine`** opens the DB: constructs `DiskManager` → `Wal` → builds the
  `BufferPoolManager` (with an `Arc<Wal>` handle for WAL-before-page) → runs
  recovery → `TransactionManager` → `BTreeIndex::open`. Owns the checkpoint/vacuum
  loop. (Exact ordering in §12.)
- **`db-core`** exposes CLOG-mutation hooks (`mark_committed`/`mark_aborted`)
  that `engine` calls **after** the corresponding WAL record is durable. It gains
  no dependency on `storage` or `Wal`.
- **`storage`** buffer pool gains an `Arc<Wal>` (or a `flush_up_to` callback) so
  the single flush seam (`write_frame_to_disk`) can enforce WAL-before-page,
  including on the eviction path.

This is the **cleanest** option — not the only one. The cycle could also be broken
by moving `Wal` down into `common` (it depends only on `DiskManager`, `Lsn`, and
the common `WalError`), or by injecting a `common`-defined `flush_up_to`/`append`
trait that `engine` implements. A dedicated `engine` crate is still preferred: it
gives startup-recovery and the checkpoint/vacuum loop a natural home, keeps `Wal`
next to the recovery code that drives it, and leaves the existing
`storage → db-core` edge intact. (The rejected alternatives are recorded so the
choice stays legible.)

---

## 3. Locked design decisions

1. **Redo granularity = hybrid.** **Physiological** (small) records for leaf
   tuple ops (`Insert`/`SetXmax`); **full-page images (FPI)** for internal-page
   ops, `LeafSplit`, `InternalSplit`, `NewRoot`, and vacuum `PageCompact`; plus a
   standard first-dirty-after-checkpoint FPI for torn-write repair. Rationale:
   internal pages already rewrite whole sections, the split heuristic isn't
   cleanly replayable, and `compact` reads live transaction-manager state — so FPI
   is the only sound form for those, while the hot leaf path stays small.
2. **`PageCompact` = FPI, and it advances the page LSN.** The compacted page is
   logged as a full image (it depends on live `tm` state and isn't reproducible
   from a logical record), and advancing the page LSN means a stale `Insert`
   cannot resurrect a vacuumed tuple (§27).
3. **Self-describing record framing** — a `rec_len`-prefixed, block-reference
   frame (§8). `rec_len` bounds allocation before the CRC check and makes unknown
   types skippable; per-type / per-block payloads carry either physiological data
   or an FPI. **LSN is a logical counter**, not a WAL byte offset (with a noted
   reconsideration for when segmentation lands — §10).
4. **Payloads carry `xmin`/`xmax` explicitly** in the block data (not derived from
   the header `txn_id`), so a record is self-describing and unit-testable in
   isolation; the header `txn_id` is then used only for CLOG (Commit/Abort) and
   ownership.
5. **A new top-level `engine` crate** owns the WAL, recovery, and checkpointing
   (§2).
6. **CLOG truncation is gated on physical reclamation** — committed entries drop
   at the global horizon; aborted entries only after vacuum has removed their
   versions (§27). This fixes the CLOG-truncation ordering bug and the
   stale-redo resurrection hazard.

> **Sequencing tip — ship pure-FPI first.** The hybrid cut (decision 1) is the
> *end-state*, not the first milestone. The physiological leaf path is a second
> replay branch plus the slot-addressing-under-reorg correctness burden — the
> likeliest place to get redo wrong. Get a correct, recoverable engine working
> with **FPI for every record** first (splits already are; just have `Insert`/
> `SetXmax` carry an FPI block too), then introduce physiological `Insert`/
> `SetXmax` as a measured size optimization. The on-disk framing already supports
> both, so this is purely a sequencing choice, not a design change.

Still to pin during implementation (not format-level): an optional per-record
`prev_lsn` (log-chain integrity / backward scan — *not* needed for redo, since
gating uses page LSN), and WAL **segmentation/retention** (single file vs
fixed-size segments) which interacts with checkpoint-driven truncation (§33).

---

## 4. Cross-cutting invariants

The rules every part of this design must honor:

1. **WAL-before-page (WBL):** a page must not reach disk before the WAL record
   that dirtied it is durable. Enforced at the single flush seam
   `write_frame_to_disk`: read `page.lsn`, `wal.flush_up_to(page.lsn)` (advances
   **`FlushedLSN`**, the durable-LSN watermark), then `write_page`. Covers
   evictions. (Full mechanism: §10.)
2. **PageLSN gating:** every mutation stamps `page.set_lsn(record.lsn)` at the
   mutation site; redo applies a record to a block **iff `record.lsn > page.lsn`**.
   This includes **page-rebuild** sites: `compact` and *both* split rebuilds must
   stamp the *new* record's LSN, never preserve the old one (§26).
3. **Commit durability:** a `Commit` record must be fsync-durable **before** the
   commit is observable in CLOG (no visible-but-not-durable commit).
4. **Single "absent-from-CLOG" convention:** `is_committed`, `is_vacuumable`, and
   recovery must agree on what a missing CLOG entry means. Aborted entries are
   retained until vacuum has physically removed all versions stamped with that id
   (§27).
5. **Checkpoint bounds replay:** redo starts at the last checkpoint's redo-point;
   the checkpoint snapshots `next_txn_id`, active txns, the vacuum horizon, the
   root pointer, and `next_page_id` (§15).

---
---

# Part II — Write-ahead log

The WAL is FluxDB's durability and crash-recovery substrate. This part captures
the **record set**, the **wire format**, the **LSN/durability primitives**, and
the **on-open corruption handling** we've settled on.

## 5. Why the record format changed

The original flat record was a single-page, key-carrying shape:

```
| lsn(8) | type(1) | key_len(8) | value_len(8) | txn_id(8) | page_id(8) | key | value | crc(4) |
```

It cannot express what redo now needs:

1. **Multi-page atomic steps.** A split / empty-page deletion / new-root modifies
   ≥2 pages and must replay all-or-nothing.
2. **Slot-addressed deletes.** Under MVCC a key can match several versions on a
   page; a delete must name the exact version. Because reorganizations are logged
   and replayed in LSN order, a version is addressable by `(page_id, slot_id)` —
   smaller and unambiguous (a key is not).
3. **Reorg logging.** Vacuum compaction and splits renumber slots; they must be
   logged so slot references in other records stay valid on replay.

So the format becomes **typed, multi-block, and slot-addressed**. This is a
**breaking on-disk change** — existing WAL files are not readable by this version.

---

## 6. The record set, derived from mutation sites

Every record type exists because some redo step or vacuum step needs it; that
traceability is the point of this section. Each record is justified by a concrete
code path. `[FPI]` = carried as a full-page image; `[phys]` = small physiological
record. (Byte layouts in §9; how each replays in Part III.)

| Category | Record | Implied by | Form |
|---|---|---|---|
| Transaction | `Commit(txn)` | `tm.commit` | phys |
| Transaction | `Abort(txn)` | `tm.abort` | phys |
| MVCC data | `Insert` (page, slot, key, val, xmin) | `LeafPageMutator::insert` + `set_xmin` | phys |
| MVCC data | `SetXmax` (page, slot, xmax) | delete + update-old-version | phys |
| Structural | `LeafSplit` | `split_leaf_ly` (3 pages) | FPI |
| Structural | `InternalSplit` | `split_internal_ly` | FPI |
| Structural | `InsertDownlink` | parent separator insert | phys |
| Structural | `NewRoot` (+ page-0 root update) | root split + `meta::set_root` | FPI |
| Structural | `MarkHalfDead` / `UnlinkPage` | empty-page deletion (§23) | phys/FPI |
| Allocation | `PageAllocate` | `new_page` (every split allocates) | phys |
| Vacuum | `PageCompact` | `compact` (live-tm dependent) | FPI |
| Recovery | `Checkpoint` | engine checkpoint loop | phys |
| Recovery | `Fpi` (standalone) | first dirty page after checkpoint | FPI |

Notes:
- **Update is not a record type** — it's `SetXmax(old)` + `Insert(new)` under one
  txn (matches `index.rs::update`).
- **`SetRoot` is not separate** — the page-0 root-pointer update is a second block
  inside `NewRoot` (and re-asserted in `Checkpoint`).
- **FPI is also a per-record flag**, not only a standalone type: any page-touching
  record may carry an FPI for its block on first-dirty-after-checkpoint (the
  decision rule — `page.lsn <= redo_point`, evaluated at the clean→dirty edge — is
  in §10).

---

## 7. Record types

`WalEntryType: u8`. **Form** reflects the locked **hybrid** redo decision (§3):
`phys` = small physiological record (page+slot+field); `FPI` = the block(s) carry
a full-page image (used where logical replay isn't sound — internal rewrites,
splits, new-root, live-`tm`-dependent compaction).

| Value | Type | Group | Form | Purpose |
|:---:|---|---|:---:|---|
| 0 | `Insert` | data | phys | insert tuple `(key, value, xmin)` at a slot |
| 1 | `SetXmax` | data | phys | stamp `xmax` at `(page_id, slot)` — slot-addressed, no key |
| 2 | `Commit` | txn | phys | mark txn committed in CLOG |
| 3 | `Abort` | txn | phys | mark txn aborted in CLOG |
| 4 | `LeafSplit` | SMO | FPI | atomic leaf split (left + right + old-neighbor prev-fix) |
| 5 | `InternalSplit` | SMO | FPI | atomic branch-page split |
| 6 | `InsertDownlink` | SMO | phys | insert separator+child into parent; clear child INCOMPLETE_SPLIT |
| 7 | `NewRoot` | SMO | FPI | new root on height increase + page-0 root update |
| 8 | `PageAllocate` | alloc | phys | record an allocation so recovery advances `next_page_id` |
| 9 | `PageCompact` | vacuum | FPI | vacuum repack (depends on live txn state → FPI) |
| 10 | `MarkHalfDead` | reclaim | phys | step 1 of empty-page deletion |
| 11 | `UnlinkPage` | reclaim | FPI | unlink an empty page from siblings + parent |
| 12 | `Checkpoint` | recovery | phys | redo-point + snapshot (see §15) |
| 13 | `Fpi` | recovery | FPI | standalone first-dirty-after-checkpoint full-page image |

**Not record types:**
- **Update** = `SetXmax(old version)` + `Insert(new version)` under one txn — no
  dedicated variant (matches `index.rs::update`). The pair is **order-dependent**:
  the `Insert` may relocate the very slot the `SetXmax` named, so `SetXmax` carries
  the *pre-insert* slot and the two must replay in LSN order (they do — same txn,
  ascending LSNs).
- **`PageAllocate` is only load-bearing across a crash between an allocation and
  the structural record that consumes it.** Every `new_page` is immediately
  followed by a `LeafSplit`/`InternalSplit`/`NewRoot` FPI that *names* the new
  page, so `1 + max page_id touched` (§17) already advances the recovered counter.
  Keep `PageAllocate` only if an allocation can become durable before its
  structural record — in which case it must be logged **and flushed before the
  page id is handed out**; otherwise it is redundant and can be dropped from v1.
- **SetRoot** is not separate — the page-0 root-pointer update is a block inside
  `NewRoot` (and re-asserted by `Checkpoint`).
- **FPI is also a per-block flag**, not only the standalone `Fpi` type: any
  page-touching record may carry an FPI for its block on first-dirty-after-checkpoint.
- `INCOMPLETE_SPLIT` / `HALF_DEAD` are **page-header flags** (reserved byte at
  offset 1), not records.

**Scope:** v1 = `Insert`/`SetXmax`/`Commit`/`Abort` + `LeafSplit`/`InternalSplit`/
`InsertDownlink`/`NewRoot`/`PageAllocate`/`PageCompact`/`Fpi` + the framing below
(makes the index crash-recoverable). v2 = `MarkHalfDead`/`UnlinkPage`/`Checkpoint`
(empty-page reclaim + bounded recovery).

> **Is the standalone `Fpi` (type 13) needed?** Every page mutation already produces
> a typed record that can carry `HAS_FPI` on its block (§6, §10), so a separate
> `Fpi` type only earns its place for a page dirtied with **no logical record** —
> e.g. an MVCC hint-bit / `is_committed`-cache write that changes bytes but logs
> nothing (PostgreSQL's `XLOG_FPI_FOR_HINT`). If FluxDB has no such unlogged page
> changes, type 13 is **redundant** — drop it and rely on the per-block flag. Name
> the unlogged-write path or remove the type before building it.

---

## 8. Wire format — generic block-reference framing

One record = a common header + N block references (each names a page and carries
its redo payload, optionally a full-page image) + a type-specific main-data blob
+ CRC. Rationale: multi-block atomicity and FPI are first-class and uniform, the
framing parser is written once, and new record types never touch it. (This
mirrors PostgreSQL's `XLogRecord` design.)

All integers little-endian.

```
WAL record =
  ┌ RecordHeader (24 bytes) ───────────────────────────────────┐
  │ lsn       u64   this record's LSN                            │
  │ rec_len   u32   total bytes (header + blocks + main + crc)   │
  │ type      u8    WalEntryType                                 │
  │ nblocks   u8    number of block refs (0 for Commit/Abort)    │
  │ txn_id    u64   owning txn (0 = system / SMO)                │
  │ main_len  u16   length of MainData                           │
  └─────────────────────────────────────────────────────────────┘
  ┌ BlockRef × nblocks (11 bytes + payload each) ──────────────┐
  │ page_id    u64                                               │
  │ blk_flags  u8    bit0 HAS_FPI, bit1 HAS_DATA                 │
  │ data_len   u16   redo-payload length for this block          │
  │ [ fpi:  PAGE_SIZE bytes ]   present iff HAS_FPI              │
  │ [ data: data_len bytes  ]   present iff HAS_DATA             │
  └─────────────────────────────────────────────────────────────┘
  MainData  [main_len bytes]     type-specific, not tied to a page
  crc        u32                 CRC32 over all preceding bytes
```

- `rec_len` lets the iterator find the record boundary and validate length.
- A block may carry **FPI**, **data**, or **both** (FPI for torn-write protection
  on first touch after a checkpoint; data for the incremental redo).
- The reader loops `nblocks` times, then reads `main_len` bytes, then the CRC.

---

## 9. Per-type payloads

`D` = block carries `HAS_DATA` (physiological); `F` = block carries `HAS_FPI`
(full-page image). Per the hybrid decision (§3), structural/compaction records use
FPI blocks; leaf tuple ops are physiological.

| Type | form | blocks | block payload(s) | main data |
|---|:---:|---|---|---|
| `Insert` | phys | 1 leaf `D` | `slot u16, key_len u16, val_len u16, xmin u64, key, val` (xmax = 0) | — |
| `SetXmax` | phys | 1 leaf `D` | `slot u16, xmax u64` | — |
| `Commit` / `Abort` | phys | 0 | — | — (subject = header `txn_id`) |
| `LeafSplit` | FPI | 3 `F` | left page, new right page, old-right-neighbor (prev-link fix) — full images | — |
| `InternalSplit` | FPI | 2 `F` | left page, new right page — full images | — |
| `InsertDownlink` | phys | 2 | parent `D`: `at_index u16, sep_len u16, right_child u64, sep_key`; left-child `D`: clear `INCOMPLETE_SPLIT` | — |
| `NewRoot` | FPI | 2 | new root page `F`; page-0 meta `D`: `root_page_id u64` | — |
| `PageAllocate` | phys | 0 | — | `page_id u64` |
| `PageCompact` | FPI | 1 `F` | compacted leaf — full image (advances page LSN) | — |
| `MarkHalfDead` | phys | 1 `D` | set `HALF_DEAD` | — |
| `UnlinkPage` | FPI | 2–3 | left sib `D`/`F`: `new_rightlink`; right sib `D`/`F`: `new_prev`; parent `D`: `remove_index` | deleted `page_id u64` |
| `Checkpoint` | phys | 0 | — | `redo_point lsn, next_txn_id, vacuum_horizon, root_pid, next_page_id` (no active-txn set — see §15) |
| `Fpi` | FPI | 1 `F` | full page image (first-dirty-after-checkpoint) | — |

### Concrete diagrams

**`SetXmax`** (≈49 bytes — the payoff of slot-addressing, no key on the wire):
```
| lsn 8 | rec_len 4 | type=1 | nblocks=1 | txn_id 8 | main_len=0 |     ← 24
| page_id 8 | blk_flags=HAS_DATA | data_len=10 | slot u16 | xmax u64 |  ← 21
| crc 4 |
```

**`LeafSplit`** (multi-block, atomic, FPI per page):
```
| lsn | rec_len | type=4 | nblocks=3 | txn_id | main_len=0 |
BlockRef[0] left     : page_id_L | flags=HAS_FPI | data_len=0 | <4 KB image>
BlockRef[1] right    : page_id_R | flags=HAS_FPI | data_len=0 | <4 KB image>
BlockRef[2] neighbor : page_id_N | flags=HAS_FPI | data_len=0 | <4 KB image>
| crc |   ← one record, three pages: redo applies all or none
```
(FPI makes the split trivially idempotent under the page-LSN gate and avoids
re-running the duplicate-key-preserving split heuristic during replay. The cost is
~12 KB per leaf split — acceptable since splits are rare relative to the leaf
`Insert`/`SetXmax` hot path, which stays physiological.)

---

## 10. LSN allocation, `FlushedLSN` & WAL-before-page

This section owns the LSN/durability primitives. Both the allocator and WBL are
**not yet built** — today `Wal::append(&mut self, lsn, …)` still *takes* an LSN and
returns `lsn + 1`, the struct has no atomic counter, `set_lsn` only *preserves* an
LSN, and the buffer pool has no `Wal` handle. The rules below are the target.

**LSN allocation.**
- `Wal` will hold `lsn: AtomicU64`. `append` claims the next LSN via `fetch_add`
  before any I/O; the caller stops passing an LSN in and receives the assigned one
  back.
- On open, scan the existing WAL to resume the counter at `max_lsn + 1` (see §11
  for how the scan handles corruption). An existing-but-empty file resumes at `0`,
  consistent with a non-existent file.

**Page-LSN stamping at the mutation site.** Whoever writes the page also writes the
LSN of the record that justifies the change:
```
lsn = wal.append(record);     // allocate + buffer the redo record
mutate page bytes;
page.set_lsn(lsn);            // page now claims "I reflect changes up to lsn"
```
The mutation site is the only place that knows the record's LSN, so stamping lives
there (in the index/engine mutators), not in the buffer pool. Redo then **applies a
record iff `record.lsn > page.lsn`** (equivalently, skips when
`page.lsn >= record.lsn`) — this is the sole source of idempotency.

**Full-page-image (FPI) decision — the same clean→dirty seam.** A page-touching
record attaches an FPI to its block (§8–9) on the **first modification of that page
since the last checkpoint**, for torn-write protection. The test is **`page.lsn <=
checkpoint_redo_point`** — the page hasn't been full-page-imaged since the
checkpoint, so a torn write of it isn't otherwise recoverable (this is PostgreSQL's
`RedoRecPtr` check). Crucially this is the **same event** as recording `rec_lsn` for
the checkpoint (§15, §33): both fire on the **clean→dirty edge** at the mutation
site, where the new record's LSN *and* the page's prior `page.lsn` are both known.
Implement them as one hook — e.g. a `mark_dirty(lsn)` that, on a clean→dirty
transition, records `rec_lsn = lsn` and decides FPI by comparing the *old*
`page.lsn` against the current `redo_point`. The FPI mechanism and the recLSN
mechanism are not independent; they are two outputs of this one edge.

**`FlushedLSN`.** The WAL tracks `FlushedLSN` — the highest LSN known durable on
disk. WBL and commit both compare against it; `flush_up_to(lsn)` advances it
(no-op when `lsn <= FlushedLSN`).

**WAL-before-page (WBL), enforced at the single flush seam.** `write_frame_to_disk`
(`shard.rs`) is the single intended path that writes a page (flush *and* eviction);
any other page-writer — e.g. a `delete_page`/recycle path — must route through this
same seam rather than writing directly. Before writing, force the log durable to
the page's LSN:
```
fn write_frame_to_disk(frame, page_id):
    let lsn = read OFF_LSN from the frame
    wal.flush_up_to(lsn)?        // <-- no page outruns its redo record
    stamp_checksum(buf); write_page(page_id, buf)   // NO per-write data fsync — see below
```
This closes the eviction hole (a dirty victim can't be flushed ahead of its log
record). The shard gains an `Arc<Wal>` so it can call `flush_up_to`.

**No data fsync on the eviction path.** A page write here must **not** `fsync` the
data file. WBL only requires the *log* durable before the page write (the
`flush_up_to` above); the data page itself needs to be durable only by the time a
checkpoint advances `redo_point` past its changes (§15). A per-eviction `fsync`
forces a slow device flush on every frame replacement and destroys throughput —
PostgreSQL deliberately batches data fsyncs at checkpoint instead. So the data-file
fsync moves to the checkpoint (§15 step 3): an evicted page sits in the OS page
cache and the checkpoint's single batched fsync makes it durable before `redo_point`
moves past it.

> **Sequencing caveat.** The current `write_frame_to_disk` (`shard.rs:319`) calls
> `sync_data()` on *every* write because, pre-WAL, that fsync is the **only**
> durability the engine has. It can be dropped **only once** the WAL exists *and*
> the checkpoint performs the batched data fsync — otherwise an evicted dirty page
> can be lost with no redo source to replay it.

**WBL ≠ commit durability — two distinct rules.** WBL forces the *data* record
durable before its page reaches disk; it does **not** force the txn's `Commit`
record durable. A page may legitimately land ahead of its `Commit` (the txn is then
lost on crash — acceptable, since "no `Commit` ⇒ not committed"). Commit durability
is the separate rule (invariant 3, §4) that a `Commit` must be fsync-durable before
the commit is observable in CLOG. Both compare against `FlushedLSN`, but they are
enforced at different seams — WBL at `write_frame_to_disk`, commit durability at
`tm.commit`.

> **LSN form — logical counter vs WAL byte-offset (open reconsideration).** We
> chose a logical counter (§3 decision 3). Note that **PostgreSQL's LSN *is* the
> byte offset into the WAL stream** (`pg_lsn`), which makes `FlushedLSN >= PageLSN`
> a direct "are these bytes on disk?" comparison and makes `flush_up_to`
> self-describing — at the cost of coupling the LSN to segment layout and
> variable-record math. Since we model Postgres closely, treat byte-offset LSNs as
> an open reconsideration for when segmentation lands (§31), not a closed door. At
> the serial baseline, a logical counter needs no counter→offset map: `flush_up_to`
> can flush-all + advance the watermark.

---

## 11. On-open / corruption handling

`append` **validates payload shape per type** and errors instead of silently
dropping or persisting stray data:
- `Insert` requires key+value; `SetXmax` carries `(slot, xmax)` and no key/value;
  `Commit`/`Abort` carry neither. (These are caller-argument errors — a dedicated
  `WalError::InvalidRecord` reads better than reusing a generic corrupted-log
  error.)

The on-open LSN scan **fails fast on mid-log corruption** but must treat a **torn
trailing record** as recoverable — and that includes a **bad CRC at the end of the
file**, not only an unexpected EOF. The CRC sits at the *end* of each record, so a
record whose bytes are physically present but partially written surfaces as a
`ChecksumMismatch`, not an EOF (the existing test in `wal.rs` proves an incomplete
final record yields `ChecksumMismatch`). So classify by **position**, not error
kind:
- `Ok(entry)` → track max LSN, continue.
- `Err(UnexpectedEof)` **or** `Err(ChecksumMismatch)` / `Err(unknown type)` **at
  the physical tail** (nothing valid parses after it) → a torn final record from a
  crash; **truncate at the last good boundary and resume.** In an append-only log a
  partial/garbled record can only be the tail.
- the **same errors with a validly-parsing record after them** → genuine mid-log
  damage → **return the error**; never skip past it (that would append after
  garbage and reuse LSNs).

Distinguishing the two needs a bounded look-ahead: on a bad record, try to resync
at the next `rec_len` boundary; if nothing valid parses through to EOF, it was the
tail. (A naive "unexpected-EOF = tail, every other error = hard fail" rule is
**unsound** — it would refuse to open a perfectly recoverable DB whose last write
was torn.)

> **⚠ DECISION PENDING — WAL framing anchor.** The tail-vs-mid-log classification
> above assumes we can find the *next* valid record after a bad one ("does anything
> valid parse through to EOF?"). In a pure byte stream with a possibly-corrupt
> `rec_len`, that is **not reliably decidable** — there is no anchor to resync on, so
> "resync at the next `rec_len` boundary" is optimistic. Resolve before building
> recovery, two ways to make it sound:
> - **(a) Add a resync anchor** — a record-start magic and/or a `prev_lsn`
>   back-pointer chain (PostgreSQL's `xl_prev`). `prev_lsn` is *not* needed for redo
>   (gating uses page LSN, §18), but it is exactly what makes this distinction
>   decidable. Cost: a few bytes per record + maintaining the chain.
> - **(b) Keep the byte stream, simplify the rule** — in a single-appender,
>   append-only log a torn write can only be at the **tail**, so: first bad CRC →
>   truncate at the tail and resume. But then **mid-log damage must HALT (fail loud),
>   never silently truncate** — silently dropping every record after a mid-log
>   bit-flip would discard committed transactions. Mid-log rot is rare and would also
>   surface at the data-page checksum layer.
>
> Pick (a) for robustness or (b) for simplicity; do **not** ship the current
> optimistic middle ground.

---
---

# Part III — Recovery

How FluxDB returns to a correct, durable state after a crash. Model: **redo-only,
no undo** (§1). There is no undo pass and no CLRs.

## 12. Ownership & startup sequence

The `engine` crate (§2) runs recovery in its constructor, before any client can
read or write. **The buffer pool must exist before the redo pass** — redo mutates
pages through `fetch_for_redo`/`set_lsn`, which are pool operations, so recovery
cannot precede pool construction:

```
Engine::open(path):
  1. DiskManager::new(path)
  2. Wal::open(wal_path)                       // scan tail, resume LSN, locate redo-point (§15)
  3. BufferPoolManager::new(disk, Arc<Wal>)    // pool exists FIRST; can enforce WBL (§10)
  4. recover(pool, wal): one forward scan from redo-point to end-of-log:
       - redo each record/block via fetch_for_redo (§13), extending the file for holes (§17)
       - rebuild in-memory CLOG from Commit/Abort (§14)
       - track watermarks: max txn_id over ALL record headers, max page_id touched (§14, §17)
  5. inject recovered watermarks (constructors discard them otherwise — see below):
       pool.set_next_page_id(max(superblock.next_page_id, 1 + max page_id))    (§17)
       next_txn_id = max(superblock.next_txn_id, 1 + max txn_id seen)          (§14)
  6. TransactionManager::from_recovered(clog, next_txn_id)
  7. BTreeIndex::open(pool)                     // reads root from the (recovered) superblock
  8. spawn checkpoint + vacuum loop
```

No reads are served until step 7. Visibility is **CLOG-authoritative-first**
(`transaction.rs`): a partially-rebuilt CLOG yields wrong answers, so CLOG must be
fully repopulated before serving. The redo pass itself does **not** consult
CLOG/visibility, so the CLOG rebuild and the redo apply share the single step-4
scan (no ordering constraint between them); the orderings that matter are
**pool-before-redo** and **full-CLOG-before-serving**.

> **Constructors must accept recovered state.** Today `BufferPoolManager::new`
> hard-sets `next_page_id = num_pages()` (already wrong once holes exist) and
> `TransactionManager::new` hard-sets `next_txn_id = 1`, with **no setter for
> either** — recovery would compute the watermarks in step 4 and the constructors
> would silently discard them. Add `from_recovered` constructors (or a
> `set_next_page_id`) so step 5 can inject them.

---

## 13. The redo pass

Single forward scan from the redo-point to the end of the valid log. For each
record, for each block it references:

```
for record in wal.iter_from(redo_point):
    for blk in record.blocks:
        page = fetch_for_redo(blk.page_id)         // recovery-only: extends the file for holes (§17)
        if record.lsn <= page.lsn:    continue     // already applied — idempotent skip
        if blk.has_fpi:               page.bytes = blk.fpi        // torn-page repair / structural redo
        else:                         apply_physiological(blk, page)  // Insert/SetXmax/InsertDownlink/...
        page.set_lsn(record.lsn)
    if record.type in {Commit, Abort}:  update rebuilt CLOG (§14)
```

- **Idempotency** comes entirely from the `record.lsn <= page.lsn` gate + per-page
  LSNs. Replaying the log twice is safe.
- **FPI blocks** overwrite the whole page (no gate needed beyond the LSN check) —
  this is how internal splits, new-root, and compaction redo, and how any torn page
  is repaired.
- **Physiological blocks** are the cheap leaf ops (`Insert`, `SetXmax`,
  `InsertDownlink`) addressed by `(page_id, slot)`. Their replay is safe because any
  compaction that renumbered slots was itself logged (as an FPI) with a higher LSN
  and is replayed in order (§26).
- **`PageAllocate`** advances the recovered `next_page_id` (§17).

Transactions with no `Commit` record (in-flight at crash) are treated as aborted —
their tuples are invisible via MVCC and reclaimed by vacuum. **No undo pass.**

**Hole pages need a recovery-only fetch path.** `new_page` doesn't grow the file,
so the log may reference a `page_id` past the current file end. The normal
`fetch_page`/`fetch_page_mut` **cannot** serve these — they call `check_page_id`
first, which returns `PageNotFound` for any `page_id >= next_page_id`, and
`read_page` uses `read_exact_at` (which errors at EOF rather than returning zeros).
Recovery therefore needs a dedicated **`fetch_for_redo(page_id)`** that bypasses the
bound check and **extends the file** with zero-filled pages up to `page_id`. A
freshly materialized hole reads as `page.lsn = 0`, so the first record that
references it always applies; an all-zero hole also passes the checksum's
unknown-type arm. The recovered `next_page_id` (§17) is injected after the scan
(§12 step 5) so later allocations don't collide with redo-materialized pages.

---

## 14. CLOG reconstruction

CLOG is in-memory only and **empty after a crash**. Recovery rebuilds it by scanning
the WAL — **CLOG is WAL-derived; there is no separate on-disk CLOG structure**:

- Replay every `Commit(txn)` / `Abort(txn)` from the checkpoint's snapshot forward,
  repopulating `clog`.
- **Advance `next_txn_id` from ALL record headers, not just Commit/Abort
  (CRITICAL — txn-id reuse is silent visibility corruption).** A txn that wrote
  `Insert`/`SetXmax` (its id lives in the tuple's `xmin`) but never committed or
  aborted contributes no Commit/Abort record, yet can be the highest id in the log.
  If `next_txn_id` is derived only from Commit/Abort subjects, that id is
  **reissued** — and `Transaction::is_visible`'s self-branch (`transaction.rs`) then
  treats the surviving uncommitted tuple as this txn's *own* write and shows it;
  once the reuser commits, every reader sees the resurrected data. So:
  `next_txn_id = max(checkpoint.next_txn_id, 1 + max txn_id over EVERY record header
  — Insert/SetXmax/PageAllocate/Commit/Abort/…)`. The checkpoint value is a floor,
  never the sole source.
- The checkpoint's `redo_point` (together with `next_txn_id`) bounds how far back the
  scan must go — *not* an active-txn set (FluxDB's checkpoint snapshots none; see §15
  for why).
- **"Absent ⇒ committed" convention (the CLOG-truncation rule):** a txn id below the
  horizon that is absent from CLOG is treated as committed. Therefore **aborted ids
  must remain in CLOG until vacuum has physically removed every version they
  stamped** — recovery must not "forget" an aborted id whose tuples still exist.
  `is_committed`, `is_vacuumable`, and recovery all use this one convention (§27).
- A txn with neither Commit nor Abort in the log (in-flight at crash) is implicitly
  aborted — no record needed, because "no Commit ⇒ not committed."

---

## 15. Checkpoints

A checkpoint bounds the redo scan and the WAL retention. FluxDB uses **non-blocking
(fuzzy) checkpoints**: writers continue; the checkpoint records a **redo-point**
(the LSN from which redo must start) and snapshots the metadata needed to interpret
the log.

**A `Checkpoint` record snapshots:**
- `redo_point` LSN (oldest recLSN among dirty pages at checkpoint start),
- `next_txn_id`,
- the **vacuum horizon** (`global_xmin`),
- the **root page id** and **`next_page_id`** high-water mark.

> **Why no active-txn set (unlike ARIES).** ARIES snapshots the transaction table at
> checkpoint to seed the undo loser set — FluxDB has **no undo**, so there is nothing
> to seed. CLOG is rebuilt by replaying `Commit`/`Abort` from `redo_point` forward;
> entries below the horizon come from the "absent ⇒ committed" default (§27);
> `next_txn_id` is snapshotted directly; and after recovery there are **no active
> survivors** (every in-flight txn is implicitly aborted), so `global_xmin` on
> restart is just `next_txn_id`. No consumer reads an active set, so it is dropped
> from the record. If one is ever genuinely needed (e.g. reporting in-progress txns
> across restart), add it back deliberately with a named consumer.

**Dirty-page tracking (recLSN) does not exist yet** — `FrameMetadata` has a bare
`is_dirty: bool`. To compute a correct `redo_point` we need, per dirty frame, the
**recLSN** = the LSN of the record that *first* dirtied it since it was last clean.
Add `rec_lsn: Option<Lsn>` to `FrameMetadata`, set on the clean→dirty edge.
`redo_point = min(rec_lsn)` over dirty frames; if none dirty, the checkpoint's own
LSN. (Note: recLSN must be driven from the mutation site where the LSN is known, not
from the page guard — see §33.)

**Checkpoint procedure (fuzzy):**
```
1. note redo_point = min recLSN over currently-dirty frames (or current LSN)
2. write Checkpoint record (snapshot above); flush WAL
3. write all currently-dirty pages (honoring WBL), then **fsync the data file once**
   for the whole batch — but DON'T block new writes. This batched fsync is what
   makes per-eviction data fsync unnecessary (§10): it guarantees every page dirtied
   since the last checkpoint is durable before redo_point advances past it.
4. durably record the new checkpoint location in the superblock via
   DiskManager::atomic_write_file (temp + fsync + rename + dir-fsync)
5. WAL before redo_point may now be recycled/truncated
```
The superblock (page 0) update for the checkpoint pointer uses `atomic_write_file`
(crash-atomic) rather than an in-place page write, since it's the bootstrap pointer
recovery reads first.

**The runtime root-pointer update must be crash-atomic too.** The root pointer in
page 0 is updated not only at checkpoint but on every height increase (root split).
The design routes that through the `NewRoot` FPI record + redo; **until that lands,
the live code is unsafe** — `index.rs` advances the in-memory root *before*
durability and then rewrites page 0 in place via `fetch_page_mut(0)` +
`flush_page(0)` (marked `//TODO: Integrate this with WAL later`), so a torn page-0
write yields `CorruptMetadata` and an unopenable DB with no fallback. Interim fix:
route every page-0 update through `atomic_write_file` (or a two-slot versioned
superblock), and don't publish the new in-memory root until page 0 is durable.

---

## 16. Incomplete SMOs (splits / page deletion)

Multi-page structural ops are split across records on purpose, and Lehman-Yao
right-links keep the tree correct mid-operation:

- **Split not yet linked into parent.** A crash between `LeafSplit`/`InternalSplit`
  and its `InsertDownlink` leaves a new right page reachable via the right-link with
  `INCOMPLETE_SPLIT` set on the left page (header flag, offset 1). The tree is
  *correct* (search/insert "move right" to find the key); the missing downlink is
  only a performance issue. **Completion is lazy**: the next descent that crosses an
  `INCOMPLETE_SPLIT` page finishes the downlink insert. Recovery does **not** need a
  dedicated fix-up pass (the pre-9.4 Postgres "finish splits during recovery"
  machinery is unnecessary).

  **Concurrent/duplicate completion must be made safe (currently unspecified).** Two
  descents can cross the same `INCOMPLETE_SPLIT` page at once; without exclusion they
  both insert the downlink → a duplicated separator and a corrupt parent. Also, the
  read/delete descents (`get`/`delete`/`find_leaf`) keep a single latch and build
  **no** parent stack, so they cannot complete a downlink as-is.

  > **⚠ DECISION PENDING — completion latch protocol.** Whatever protocol is chosen
  > must **never hold a descendant latch while acquiring an ancestor** outside the
  > established bottom-up split-propagation order, or it inverts the descend-only
  > discipline the tree relies on. This design *does* rely on that discipline today:
  > descent takes shared latches and releases them as it goes (collecting ancestor
  > *page-ids* in `BTStack`, not held guards), and `insert_separator_via_stack` drops
  > the parent before latching the grandparent (`index.rs:924`) — so there is no
  > top-down both-held path to deadlock against *right now*. The safe pattern
  > (PostgreSQL's `_bt_insert_parent`): **release the child latch, relocate the
  > parent via the right-link / parent stack, then latch the parent and
  > insert-the-separator-if-absent** (idempotent; redo of `InsertDownlink` likewise
  > insert-if-absent). Do **not** hold the child latch across the parent fetch. Note
  > `split_and_insert`'s left-target branch (`index.rs:743–746`) currently *does*
  > hold the leaf latch across the parent fetch inside `insert_separator_via_stack`;
  > it is safe only because everything else is strictly bottom-up, and it is the
  > fragility to clean up when this protocol is built.

  > **Build status / decision needed.** The `INCOMPLETE_SPLIT` flag (header byte 1)
  > is **not maintained today** — `split_leaf_ly`/`split_internal_ly` set no flag and
  > no descent checks one (byte 1 is documented "never used"). The rightlink "move
  > right" invariant **is** implemented, and that is what keeps the tree correct; the
  > flag is only an optimization (it tells a descent a downlink is missing so it can
  > repair eagerly instead of always moving right). Pick one before relying on the
  > lazy-completion story: **(a)** build the flag for real (set on split, clear on
  > `InsertDownlink`, check on descent — and make completion concurrency-safe as
  > above), or **(b)** drop the flag-based story and rely purely on rightlinks (search
  > always moves right; downlinks are inserted only by the normal split path). The
  > design currently assumes (a); the code only supports (b).

- **Page deletion.** `MarkHalfDead` then `UnlinkPage` are separate records; a crash
  between them leaves a half-dead page that the next vacuum pass completes. Recycling
  is horizon-gated (§23), so a not-yet-recycled deleted page is never handed out
  mid-recovery.

This is why the split/delete records are designed as separable steps rather than one
giant atomic record: it makes both runtime concurrency and crash recovery rely on the
same right-link invariant instead of special-casing recovery.

---

## 17. Page allocation recovery & hole pages

`new_page` bumps an in-memory counter and doesn't grow the file, so `next_page_id`
can't be trusted from file length once holes exist. Recovery:

```
next_page_id = max(superblock.next_page_id,
                   1 + max page_id touched by any redo record (incl. PageAllocate))
```
Any addressable page below `next_page_id` that is an all-zero hole is left as-is (it
has `page.lsn = 0`; the first real record for it will overwrite via redo) or zeroed
defensively. The superblock slot is the checkpoint-time source of truth. This
recovered value is **injected after the scan** (§12 step 5) — the pool otherwise
fixes `next_page_id = num_pages()` at construction, which is wrong once holes exist.
**`PageAllocate` is only load-bearing if an allocation can become durable before the
structural record that consumes it**; otherwise `1 + max page_id touched` already
covers it, since every `new_page` is immediately followed by a `LeafSplit`/
`InternalSplit`/`NewRoot` FPI that names the page (§9). See §13 for the
`fetch_for_redo` file-extend path that materializes holes.

---

## 18. What recovery must NOT do (consequences of no-undo)

- It does **not** roll back uncommitted transactions. Their tuples remain; MVCC
  hides them (no `Commit` in CLOG) and vacuum reclaims them.
- It does **not** write CLRs or maintain undo chains.
- It does **not** need `prev_lsn`-per-page for undo (the optional `prev_lsn` in the
  record header is for log-chain integrity / backward scan only).

---
---

# Part IV — Vacuum & space reclamation

This part is the authoritative spec for space reclamation in FluxDB's Lehman-Yao
B+Tree under PostgreSQL-style MVCC. (It supersedes any older `DELETED`-flag +
`version` model — the engine uses per-tuple `xmin`/`xmax` instead, in
`page/leaf.rs`.) Because recovery is redo-only with no undo (§1), vacuum is a
**mandatory, first-class subsystem**: it is what reclaims aborted and dead versions
and what lets the CLOG shrink.

## 19. Strategy: prevent / tolerate / reclaim

We deliberately **do not merge or rebalance underfull pages online** (the PostgreSQL
choice). A merge touches two siblings + their parent at once, which breaks the
descend-only, one-latch-at-a-time invariant the read/insert paths rely on (the
Lehman-Yao traversal in `index.rs`), adds write amplification, and fights concurrent
readers. Instead bloat is handled in three parts:

| Stage | Mechanism | Status |
|-------|-----------|--------|
| **Prevent** | Pre-split dead-tuple cleanup ("bottom-up deletion") before a split | ❌ planned (§25) |
| **Tolerate** | Leave sparse-but-non-empty pages as-is — no merge | ✅ (no work) |
| **Reclaim** | In-page compaction → delete & recycle **fully-empty** pages → file truncation → periodic bulk-load rebuild | 🟡 partial (§23–24) |

**Why this is acceptable (and where it is not):**

- Under **random/uniform keys** the tree self-heals — inserts refill sparse pages
  about as fast as deletes empty them (steady-state fill ≈ `ln 2` ≈ 69%).
- Under **monotonic keys + range deletes** (queue / time-series) sparse pages on the
  left are never refilled and stay stranded until they fully empty. This is the one
  pathology of no-merge; the relief valve is a periodic **bulk-load rebuild** (the
  REINDEX analog), *not* online merging.

So the dominant anti-bloat levers are **in-page compaction** (built) and **pre-split
cleanup** (planned) — never rebalancing.

---

## 20. What FluxDB vacuum does NOT need — and why

Being index-organized with u64 transaction ids removes three of the heaviest parts
of a real PostgreSQL vacuum. Recording them so the design isn't made to look more
daunting than it is:

### 20.1 No Free Space Map (FSM)

PostgreSQL's heap can place any tuple on any page with room, so it maintains an FSM
to answer "find me a page with N free bytes." **FluxDB is key-addressed:** an insert
descends to the *one* leaf whose key range covers it. Reclaimed in-page space is
therefore reusable **only by future inserts in that same key range** — there is no
"search for a page with room" and no FSM to maintain.

This is also the *root cause* of the stranding in §19: freed space is range-locked to
its page, so a key range that never sees another insert never reuses it.

### 20.2 No separate index cleanup

PostgreSQL vacuum is a two-structure dance: remove dead heap tuples, then remove the
matching entries from every index pointing at them. In FluxDB the **leaf is both the
data and the index** — there is one structure. Stripping a dead version from the leaf
*is* the whole job. The entire heap↔index coordination phase (and its bulk-delete
machinery) does not exist here.

### 20.3 No tuple freezing, no xid wraparound

PostgreSQL freezes old tuples (rewrite `xmin` → a "frozen / always visible" sentinel)
for two reasons: (a) its xids are **32-bit** and wrap at ~4 billion, so old xids must
be retired before reuse; (b) to let the commit log (CLOG) be truncated.

FluxDB's `txn_id` is **u64** (`next_txn_id: AtomicU64`). It will not wrap in any
realistic lifetime, so reason (a) is gone entirely. Reason (b) is also gone: because
there is no wraparound, the rule **"`txn_id < global_xmin` and not recorded aborted
⇒ committed"** is permanently sound (`Snapshot::is_committed` already encodes the
`< xmin ⇒ committed` default). So **committed** CLOG entries below the horizon can be
dropped *without* rewriting any tuple — the default re-derives their status
correctly.

**Conclusion: FluxDB does not implement tuple freezing at all.** The only CLOG
constraint that remains is the *aborted*-entry ordering rule in §27.

---

## 21. What exists today

### `BTreeIndex::vacuum(&self, tm)` — `index.rs:73`

Walks the leaf chain from the leftmost leaf via `rightlink()`, taking one exclusive
latch at a time, and calls `compact()` on each page. Returns the total number of dead
versions removed.

```rust
let global_xmin = tm.global_xmin();
let mut leaf_pid = self.find_leftmost_leaf(root)?;
loop {
    let mut guard = self.pool.fetch_page_mut(leaf_pid)?;
    total_dead += LeafPageMutator::<K, V>::compact(leaf_pid, &mut guard[..], global_xmin, tm);
    let next = LeafPageAccessor::<K, V>::new(&guard[..]).rightlink();
    drop(guard);
    match next { Some(pid) => leaf_pid = pid, None => break }
}
```

### `LeafPageMutator::compact(page_id, data, horizon, tm)` — `leaf.rs`

MVCC-aware repack: gathers every record for which `is_vacuumable` is **false**,
rebuilds the page via `LeafPageBuilder` (preserving `high_key`, `rightlink`,
`prev_page`, `lsn`), drops the rest. No-op when nothing is dead.

> ⚠️ **Redo hazard to fix:** `compact` currently re-stamps the **old** LSN
> (`leaf.rs:575`). Under redo gating that's unsound — a stale `Insert` with a higher
> LSN could resurrect a vacuumed tuple. Once WAL lands, `compact` must be logged as a
> `PageCompact` **FPI** and stamp the **new** (record's) LSN so the compacted state
> strictly post-dates everything it removed (§3 decision 2; §26).

### `is_vacuumable(xmin, xmax, horizon, tm)` — `transaction.rs:200`

A version is *definitely dead* iff:
1. `tm.is_aborted(xmin)` — its creator aborted (it was never valid), **or**
2. `xmax` is committed **and** `xmax < horizon` — deleted by a committed txn older
   than the global horizon, so no live snapshot can reach the old state.

`horizon = tm.global_xmin()` = the oldest active txn id (or `next_txn_id` if none
active).

> ⚠️ **Rule 2 must use the same "committed" default as visibility, not the bare CLOG
> lookup.** Once committed CLOG entries below `global_xmin` are truncated (§27),
> `tm.is_committed(xmax)` returns `false` for a deleter whose entry was dropped — and
> the dead tuple becomes **permanently un-vacuumable** (a space leak): vacuum can't
> tell it's deletable, yet it's invisible to every reader. Use the `< xmin ⇒
> committed` default that `Snapshot::is_committed` already applies:
> `xmax_committed = tm.is_committed(xmax) || (xmax < horizon && !tm.is_aborted(xmax))`.
> This is the rule-2 mirror of the truncation asymmetry in §27 — a dropped *committed*
> entry must still read as committed.

**Not built yet:** empty-page deletion + recycle, the safe-recycle delay, file
truncation, pre-split cleanup, WAL logging of page rewrites, autovacuum
trigger/throttling, and the skip-scan optimization (all specified below).

---

## 22. Reclaim mechanism A — in-page compaction (built)

`compact()` above. This is the core primitive and the main anti-bloat lever: it
removes dead versions and repacks live ones, returning bytes to the page's free
region for future same-key-range inserts. Everything else orchestrates around it.

---

## 23. Reclaim mechanism B — empty-page deletion + delayed recycle

When `compact()` leaves a leaf with **zero** live versions, the page should be
removed from the tree and recycled — but **never merged**. Following nbtree:

1. **Mark half-dead** (a header flag, reuse the reserved byte at offset 1) so
   concurrent descents know it is going away and route via the right-link.
2. **Unlink**: splice it out of the `prev_page`/`rightlink` sibling chain and remove
   the separator/downlink from the parent. Right-links keep concurrent scans correct
   throughout (a reader on the dying page follows `rightlink`).
3. **Delay recycling.** The page id may **not** go straight onto the free list: a
   concurrent scan or older snapshot might still be walking toward it. Stamp the
   deletion with the current horizon and return the page to the free list only once
   `global_xmin()` has advanced past it (the nbtree `btpo.xact` trick). This is the
   dependency on **free-space management**: a freed B+Tree page is not immediately
   reusable under MVCC.

We stop here — **no borrow/redistribute/merge of partially-full pages.** Only
fully-empty pages are removed.

> **Hard prerequisite: crash-consistent free-space tracking (build this first).**
> This mechanism is **not** safely buildable on its own. Free-list push/pop and the
> recycle horizon must survive a crash, and an intrusive free-list chain would need
> its own WAL record plus an FPI for the link bytes. **Prefer a bitmap free-space
> page** — it's covered by the ordinary FPI/redo path, so push/pop becomes
> crash-consistent for free. Until that exists, **leak** empty pages (mark them dead
> but never recycle the page id) rather than recycle unsafely. (§33 also tracks this
> as an open item; it is restated here because §23's mechanism is incomplete without
> it.)

> **Leaf-only for v1 (internal pages have no `prev` link).** The `UnlinkPage`
> record's "right-sibling `new_prev`" block (§9) is unimplementable at the branch
> level — internal page headers carry only a `rightlink`, no backward pointer.
> Empty-**leaf** deletion is well-defined (leaves have `prev_page`);
> empty-**internal**-page deletion needs an internal prev link first. Restrict
> deletion to leaves until that exists, or add the internal `prev` pointer and say
> so. (Empty internal pages are also rarer — they arise only when an entire subtree
> empties.)

---

## 24. Reclaim mechanism C — file truncation (return space to the OS)

Empty-page deletion (§23) returns pages to the *free list* but does not shrink the
database file. When a run of pages at the **end** of the file are all free, they can
be truncated away so disk is returned to the OS (PostgreSQL does this for trailing
empty heap pages). This depends on free-space management knowing the high-water mark
and which trailing pages are free. Lower priority than §23 — without it, disk usage
plateaus at the high-water mark rather than shrinking, but space is still reused
internally.

---

## 25. Prevent — pre-split "bottom-up" deletion

The highest-value addition. When `insert()` finds the target leaf full and is about
to call `split_leaf_ly`, first try to make room by reclaiming *dead* versions on that
one page:

```
insert → leaf full?
   → run in-page compaction on THIS leaf (reuse compact())
   → enough room now?  → insert here, NO split
   → still full?       → split_leaf_ly as today
```

This attacks bloat at the instant it would otherwise be created — a split caused by
MVCC version churn (repeated updates of one key piling up dead versions). It is not a
new mechanism, just a new trigger point for `compact()` inside the insert/split path.
(Named "bottom-up" because it fires at the leaf during normal DML, not from a
scheduled sweep. Distinct from **bulk load**, the construction-time rebuild.)

---

## 26. WAL / redo invariants for vacuum & split

Constraints the recovery model imposes on vacuum and split; not yet enforced in code.

1. **No undo (PostgreSQL model).** Abort never rolls back page bytes; it writes an
   `Abort` CLOG mark. Aborted versions physically remain and are removed later by
   vacuum (`is_vacuumable` rule 1). Redo replays page changes for committed *and*
   uncommitted txns up to the crash; MVCC visibility + CLOG decide what counts
   afterward. There are **no CLRs** and no undo pass.
2. **SMOs are not transactional.** A split or empty-page deletion by txn T survives
   whether T commits or aborts. (Even textbook ARIES makes structure changes
   non-undoable via nested top actions — so this part is the same either way.)
3. **Vacuum and split mutate pages → must be WAL-logged:**
   - `compact()` is logged as a `PageCompact` **FPI** that advances the page LSN
     (it depends on live `tm` state and isn't replayable from a logical record; the
     FPI also makes a stale `Insert` unable to resurrect a vacuumed tuple).
   - `split_leaf_ly` is a multi-page atomic step → a **single multi-block WAL
     record** (FPI per page) so redo never applies half a split. The parent downlink
     insert is a separate record; a crash between is tolerated via the right-link + an
     `INCOMPLETE_SPLIT` flag, finished lazily on the next descent (§16). (The third
     page in `LeafSplit` — the old right-neighbor's `prev_page` fix — is the
     *backward* chain, which carries **no crash invariant**: only rightlinks are
     load-bearing for correctness. Folding it into the atomic FPI set keeps it tidy,
     but a torn backward link would not corrupt search.)
   - Stamp `page.set_lsn(lsn)` after every mutation; redo **applies iff
     `record.lsn > page.lsn`** (skips when `page.lsn >= record.lsn`) for idempotency.
   - **Every page-rebuild site must advance the LSN, not preserve it** — this applies
     to **all three** `Builder::finish()` callers, not just `compact`. `compact`
     (`leaf.rs`) *and* both split rebuilds (`split_leaf_ly`, `split_internal_ly` in
     `index.rs`) currently re-stamp the **old** page LSN. The splits don't *remove*
     versions (they snapshot all of them), so they have no resurrection bug per se —
     but the soundness rule is uniform: a rewritten page must carry the **new
     record's** LSN, never a stale one, or an already-applied lower-LSN record can
     re-fire on redo (and for `compact`, a stale higher-LSN `Insert` can resurrect a
     vacuumed tuple). State and enforce it once for every `finish()` site (§4
     invariant 2).
4. **Slot-addressed redo depends on ordered replay.** Because compaction and splits
   are logged and replayed in LSN order, a `SetXmax` record naming `(page_id,
   slot_id)` is interpreted against the reconstructed page state — so the slot stays
   valid even though vacuum/splits relocate tuples. This is what lets WAL delete
   records drop the key for a `slot_id` (and it's *more* correct under MVCC: a key can
   match several versions, a slot names exactly one).

---

## 27. CLOG truncation & the ordering invariant

Vacuum is what lets the CLOG shrink, and getting the ordering wrong is an active bug.

**The bug.** `truncate_clog(horizon)` currently drops *both* Committed and Aborted
entries below the horizon. After an Aborted entry is dropped, `Snapshot::is_committed`
(`transaction.rs:110`) no longer sees `is_aborted == true`, falls through to
`txn_id < xmin → return true`, and the aborted creator is read as **committed** — any
surviving record with that `xmin` becomes phantom-visible. Worse, `is_vacuumable` rule
1 *also* keys on `tm.is_aborted(xmin)`, so truncating first makes those records
simultaneously **unrecognizable to vacuum** and **visible**.

**The asymmetry (why committed and aborted differ):**

- **Committed entries** below the horizon are safe to drop anytime — the
  `txn_id < xmin ⇒ committed` default re-derives their status correctly (§20.3). This
  is the CLOG-bounding mechanism, and it needs no freezing.
- **Aborted entries** must be **retained until vacuum has removed every tuple that
  carries that `xmin`.** Only then can the entry be dropped, because after that no
  record depends on its status.

**The invariant:**

> Committed CLOG entries may be truncated at the global horizon. An Aborted CLOG
> entry may be dropped only after a completed vacuum pass has removed all records with
> that `xmin`. Equivalently: **the aborted-entry truncation horizon must trail the
> completed-vacuum horizon — never lead it.**

**Implementation (two-tier truncation):**
1. `truncate_clog` drops **Committed** (and keeps Active) below `global_xmin`.
2. A separate step drops **Aborted** entries below `vacuum_horizon` — the horizon
   that the most recent *completed* full vacuum has already swept — at which point
   their tuples are gone, so no reader can reach them.

(No tuple freezing is involved — see §20.3. Freezing is a 32-bit-xid necessity
FluxDB's u64 ids eliminate.)

### 27.1 Tracking `vacuum_horizon`

> **Status: TARGET, not current code.** `vacuum_horizon` does **not exist** as a code
> symbol yet, and `truncate_clog` (`transaction_manager.rs`) still drops *both*
> Committed and Aborted entries below the horizon — i.e. the buggy behavior is live
> (though `truncate_clog` currently has no non-test callers, so it is dormant until
> CLOG truncation is wired). Everything below is what to build.

`vacuum_horizon` is a single `AtomicU64` on `TransactionManager`, advanced only when
a **full** sweep completes:

```rust
// at the START of a fresh full vacuum cycle:
let h_start = tm.global_xmin();
// ... walk EVERY leaf, compact() each ...
// only on reaching the last leaf (full cycle complete):
tm.vacuum_horizon.fetch_max(h_start, Ordering::AcqRel);
```

Why `global_xmin` at pass start is the correct bound: it is the oldest *active* txn,
so every txn `< h_start` is already settled (committed or aborted) and its status will
never change again. The pass removes every aborted-`xmin` record (`is_vacuumable` rule
1), so once the full sweep finishes, **no aborted-`xmin` record with `xmin < h_start`
survives anywhere** → aborted CLOG entries below `h_start` are safe to drop. Because
`vacuum_horizon` lags `global_xmin`, aborted truncation trails committed truncation,
as required.

Correctness details:
- **Publish only on completion**, never mid-pass. With batched/cursor vacuum (§28),
  stash `h_start` when a cycle begins and `fetch_max` only when the cursor reaches the
  last leaf; partial progress publishes nothing.
- **A partial sweep can't be fooled:** records only ever move *right* in the rightlink
  chain (a split keeps the lower half **in place** and pushes only the upper half to a
  new right sibling), and the sweep moves left→right. So an aborted-`xmin < h_start`
  record is either ahead of the cursor (will be visited) or was already on a visited
  page (already removed) — it cannot slip behind the cursor. Nothing *creates* an
  `xmin < h_start` record (new inserts get fresh, large xids). Two conditions make
  this airtight, and the implementation must hold them: **(a)** the sweep **reaches
  every leaf reachable at `h_start`** by following rightlinks to the end (a split
  *adding* a page ahead of the cursor is fine — it's visited later or carries only
  fresh xids); **(b)** the only leaf-removing reorg, `UnlinkPage` (§23), fires **only
  on already-empty pages**, so it never relocates a live aborted-`xmin` record
  leftward past the cursor.
- **On restart:** default `vacuum_horizon = 0` (safe — just can't drop aborted entries
  until the first post-restart vacuum completes), or persist it in the metadata page
  to resume immediately. Start with 0.

### 27.2 Durability: vacuum logs the removal, not the abort

Vacuum does **not** write the aborted record to the WAL — the abort was already logged
by the `Abort` record at abort time. What vacuum logs is the physical repack:
`compact()` emits a **`PageCompact`** FPI record (§26) — a full image of the compacted
page that **advances** the page LSN.

Durable CLOG shrinkage is therefore **checkpoint-gated**, not vacuum-gated:

- CLOG is rebuilt from the WAL on recovery (replay all `Commit`/`Abort` records), so
  dropping an entry *in memory* is not a durable decision — after a restart the
  `Abort` record replays and the entry reappears.
- An aborted txn is *permanently* forgotten only when the WAL segment containing its
  `Abort` record is truncated, which happens at a **checkpoint** — once (a) the
  `PageCompact` that removed its records is durable and (b) `vacuum_horizon` has
  advanced past it.

So the full chain is: **vacuum logs `PageCompact` → checkpoint confirms pages durable
+ advances `vacuum_horizon` → pre-checkpoint WAL (incl. those `Abort` records) is
discarded → only then are those aborted txns truly forgotten.** In-memory
aborted-entry truncation at `vacuum_horizon` is the memory-bound optimization layered
on top of that.

---

## 28. Triggering & throttling (autovacuum)

`vacuum()` is currently manual and all-or-nothing. A production vacuum needs:

- **A trigger:** a dead-tuple counter (per page or global) crossing a threshold,
  rather than relying on an explicit caller. Maintain the counter on `delete`/`update`
  (each `xmax` stamp) and on aborts.
- **Bounded batches / cursor mode:** vacuum a bounded number of pages per tick and
  resume, instead of one giant sweep, so it interleaves with foreground work.
- **Cost-based back-off:** sleep between batches so vacuum doesn't starve writers for
  leaf latches (PostgreSQL's `vacuum_cost_delay` analog).
- **Stats:** pages scanned, bytes/versions reclaimed (`vacuum` already returns
  `total_dead`), empty pages deleted.

> **Operational hazard: a long-running or idle transaction pins `global_xmin`.**
> `global_xmin` is the oldest *active* txn id; a single long-lived or abandoned
> transaction holds it back, which stalls **vacuum** (nothing below the frozen
> horizon is reclaimable), **CLOG truncation** (committed entries can't drop below
> the horizon — §27), and transitively **WAL truncation** (the checkpoint can't
> advance the horizon it snapshots, so pre-checkpoint log can't be recycled). This
> is the standard MVCC bloat hazard. Mitigations a production engine needs: an
> "old-snapshot" age threshold that can cancel offenders, and monitoring of the
> oldest-snapshot age. At minimum, surface the oldest active-txn age as a stat so
> the stall is observable.

---

## 29. Skip-scan optimization (future)

Today `vacuum()` visits every leaf on every pass. A visibility-map analog — a per-page
"all live / nothing to reclaim since last vacuum" bit, cleared on any `delete`/`update`
to that page — lets vacuum skip clean pages entirely. Pure performance; defer until the
trigger/throttling (§28) exists.

---

## 30. Vacuum concurrency

- Vacuum holds **one exclusive leaf latch at a time**; between pages it holds none →
  no deadlock, foreground writers proceed on other pages.
- Following `rightlink` is stable under concurrent splits: a page inserted ahead is
  simply visited on a later cycle; nothing is lost.
- Empty-page deletion (§23) is the one place that touches >1 page; it must use the
  right-link + half-dead protocol rather than naive multi-latching.

---
---

# Part V — Roadmap & reference

## 31. Concurrency & performance — deferred WAL optimizations

Build for correctness first. The WAL is the classic high-concurrency contention
point, but none of the items below are needed for a correct, recoverable single-node
engine — apply them **after** everything works. None change correctness; several
change the segment layout, so they're noted for sequencing.

### 31.1 Baseline (where we start)
`Wal::append` takes `&mut self` → a **single serial appender**. This is the "one
global mutex" model: correct and simple, the right starting point. Everything below
relaxes it.

### 31.2 Group commit (first, highest-value win)
N transactions committing at once → N `fsync`s, each a slow device flush. Batch them
with a **leader/follower** pattern: a committing thread enqueues; the first becomes
leader, waits a few µs for others to join, issues **one** `write()`+`fsync()` for the
whole batch, then wakes the followers. Integration is cheap — the `TransactionManager`
already has the condvar `waiters` / `notify_waiters`, which *is* the follower-wake
mechanism. The correctness rule is untouched (a commit is acked only once
`FlushedLSN >= its LSN`); group commit just amortizes the fsync.

### 31.3 Concurrent WAL buffer (circular, slot reservation)
Replace the `BufWriter` with a fixed **circular buffer** mirroring the segments. A
thread takes a short latch only to **reserve** a byte range (bump the tail), releases
it, then `memcpy`s its record in parallel with other threads. Many threads fill the
buffer at once; a writer thread drains it to disk.

### 31.4 Lock-free append + the hole problem
Replace the reservation latch with an atomic **`fetch_add`** on the tail (the returned
offset is a "ticket" = an exclusive byte range); threads `memcpy` with no lock. **The
hole problem:** if thread B (range 201–300) finishes before A (100–200), there's a gap
and you can't flush past the first hole — so `FlushedLSN` advances only to the first
*unfilled* slot, tracked by a per-chunk "ready" bitmask.
> This is the **same shape** as the buffer-pool `loading`-state fix: a
> reserved-but-not-yet-valid slot you must not expose/flush past until it's filled. The
> serial baseline (31.1) sidesteps holes entirely — they appear only once append is
> concurrent.

### 31.5 Backpressure (bounded buffer)
A circular buffer is finite. If writers outrun the disk, the tail catches `FlushedLSN`
→ block new appends. That's natural backpressure (and the source of the "spiky"
latency seen under WAL pressure — the buffer draining behind an fsync).

### 31.6 Segment management
Split the WAL into fixed-size **segments**. **Pre-allocate** (create + zero-fill) the
next segment ahead of time so a rotation doesn't pay filesystem-allocation latency
under load. **Recycle** (rename for reuse) a segment only once it's entirely behind the
checkpoint **redo-point** (§15) — never recycle a segment redo still needs.
(Segmentation is also the prerequisite that makes the byte-offset-LSN reconsideration
in §10 concrete.)

### 31.7 False sharing / cache-line padding
Once `LogTail` and `FlushedLSN` are hot atomics, keep them on **separate 64-byte cache
lines** (pad) so a tail `fetch_add` doesn't invalidate the flusher's cache line and
vice-versa. Only relevant after 31.4.

### 31.8 Out of scope (single-node)
- **Sharded / per-NUMA WAL buffers** — zero cross-shard contention, but recovery must
  then merge multiple logs into one timeline (needs a global ordering).
- **Archiving / log shipping / replication slots** — don't recycle a segment until a
  replica acks. Distributed-systems features; not for a single node.

### Sequencing
**Group commit (31.2)** lands first — highest value, lowest risk, no format change.
The **concurrent/lock-free buffer (31.3–31.4)** and **segmentation (31.6)** come
together (segmentation gates the bounded buffer + recycling and the byte-offset-LSN
question). **Padding (31.7)** only matters after 31.4. **31.8** is deferred
indefinitely.

Related engine-level performance (independent of the WAL, tracked separately): an
insert fastpath for monotonic keys, range-scan read-ahead / prefetch + group/coalesced
flush, and bulk-load / REINDEX.

---

## 32. What has to be built (implementation order)

**Current reality:** none of the durability subsystem is wired. `Wal`/`set_lsn`/CLOG
are dead for recovery; there is no engine layer; `new_page` doesn't grow the file;
`compact` re-stamps the old LSN; `truncate_clog` has the ordering bug. The work, in
dependency order:

1. **`engine` crate + `Engine::open` skeleton** (no recovery yet) + `Arc<Wal>` into the
   pool (§2, §12).
2. **Log manager:** self-describing framing + record set + LSN allocator + `FlushedLSN`
   + `flush_up_to`/fsync + torn-tail semantics (§7–11).
3. **WBL + page-LSN stamping:** `Arc<Wal>` on the pool; `set_lsn` at every mutation
   site; `flush_up_to` at `write_frame_to_disk`; emit records from index mutations
   (§10, §6). *Ship pure-FPI first* (§3).
4. **Recovery pass:** redo from the redo-point (or log start, pre-checkpoint), FPI vs
   physiological apply, CLOG rebuild + the critical `next_txn_id` derivation,
   `fetch_for_redo` for holes, incomplete-split completion (§12–17).
5. **Checkpointing:** recLSN tracking (does not exist yet), `Checkpoint` record, fuzzy
   checkpoint, superblock pointer via `atomic_write_file` (§15).
6. **Vacuum hardening:** compact-as-FPI, empty-page deletion + recycle horizon, two-tier
   gated CLOG truncation with `vacuum_horizon` (§22–27).

The corresponding vacuum-side phase order: (have) in-page compaction + full-tree vacuum;
pre-split bottom-up deletion (cheap, no WAL dep); the two-tier CLOG-truncation fix (no
WAL dep); empty-page deletion + delayed recycle (needs free-space mgmt); WAL logging of
compaction & splits; autovacuum trigger + throttling; file truncation + skip-scan;
bulk-load rebuild.

**Explicitly NOT planned:** sibling borrow / redistribute / merge of partially-full
pages (§19); Free Space Map (§20.1); separate index cleanup (§20.2); tuple freezing /
wraparound handling (§20.3).

---

## 33. Open items to settle during implementation

- **recLSN tracking granularity** — per-frame `rec_lsn` (§15) vs a separate dirty-page
  table keyed by page_id. Per-frame is simpler and sufficient.
- **recLSN must be driven from the mutation site, not the page guard.** §15 says set
  `rec_lsn` on the clean→dirty edge, but the page write guard only flips a local
  `dirty` bool on `deref_mut` and has no LSN at deref time. The clean→dirty recLSN must
  be recorded where the LSN is known — the mutation site that calls `wal.append` then
  `set_lsn` (§10) — e.g. a `mark_dirty(lsn)` on the frame that records `rec_lsn` only on
  the clean→dirty edge.
- **WAL segmentation & retention** — single growing file vs fixed-size segments;
  affects checkpoint-driven truncation and `flush_up_to` (§31.6). Recovery assumes
  "redo from redo-point" works regardless. Without segmentation, the WAL is a single
  ever-growing file with no physical reclamation — recovery scan time and disk use grow
  without bound between checkpoints (checkpoints bound replay *start*, not the physical
  log).
- **Group commit** — batching `flush_up_to`/commit fsyncs under the condvar path (§31.2).
  Deferred optimization; the correctness rule (commit durable before observable) holds
  without it.
- **Crash *during* recovery (re-entrancy).** Redo is per-record idempotent via the
  page-LSN gate, but the recovery-time page writes must themselves be WBL-ordered, and a
  half-extended file / half-materialized hole must be safe to re-run from the same
  redo-point. A second crash before the first post-recovery checkpoint must replay
  cleanly.
- **Fuzzy-checkpoint mid-write consistency.** Writers continue during a checkpoint
  (§15), so the `Checkpoint` record's snapshot (active set, `next_txn_id`, horizons)
  must capture a coherent instant, and a crash *while writing the checkpoint record or
  the superblock pointer* must leave the previous checkpoint usable (the
  `atomic_write_file`/two-slot rule in §15 covers the pointer; the record itself is
  CRC-validated and ignored if it's a torn tail, §11).
- **CLOG memory bound.** CLOG is an in-memory map rebuilt from the whole
  post-checkpoint WAL. Aborted entries are pinned until vacuum sweeps their tuples
  (§27), so an abort-heavy workload can grow CLOG between checkpoints, and recovery pays
  replay cost proportional to that history. Bounded by checkpoint frequency — note it,
  don't solve it yet.
- **Free-list / recycle crash consistency.** Empty-page recycle (§23) has no WAL record
  for free-list push/pop and no FPI for the intrusive link bytes, and the MVCC recycle
  horizon lives outside the buffer pool with no reservation. Prefer a **bitmap
  free-space page** (covered by ordinary FPI/redo) over an intrusive free-list chain —
  it makes recycle crash-consistent for free via the existing redo path. Hard
  prerequisite before implementing recycle; until then, **leak** pages rather than
  recycle unsafely.
- **Per-record `prev_lsn`** — log-chain integrity / backward scan only; *not* needed for
  redo (gating uses page LSN). Pin during implementation if a backward scan is wanted.

---

## 34. Tests to add

- `vacuum_removes_aborted_xmin` — record by an aborted txn is stripped.
- `vacuum_removes_committed_xmax_below_horizon` — deleted-and-old version gone.
- `vacuum_keeps_version_visible_to_active_snapshot` — `xmax >= horizon` kept.
- `vacuum_empty_leaf_is_deleted_and_recycled` — once §23 lands.
- `recycled_page_not_reused_while_reader_active` — delayed-recycle horizon.
- `presplit_cleanup_avoids_split` — version churn on one key reclaims instead of
  splitting (once §25 lands).
- `clog_commit_truncation_keeps_old_committed_visible` — drop committed entry below
  horizon, old committed record still visible (default-to-committed).
- `clog_abort_truncation_after_vacuum_keeps_aborted_invisible` — the ordering
  regression: abort, vacuum (removes the record), then drop the aborted entry; a fresh
  snapshot must NOT see the aborted record.
- `vacuum_idempotent` / `vacuum_no_tombstones_noop` / `vacuum_empty_tree_noop`.
- `recovery_reuses_no_txn_id` — an in-flight txn that wrote tuples but never
  committed/aborted must not have its id reissued after restart (the `next_txn_id`
  derivation in §14).
- `recovery_torn_tail_opens` — a DB whose last record is torn (bad CRC at EOF) opens
  and recovers; mid-log damage halts (§11).

---

## 35. PostgreSQL reference mapping

For implementers familiar with PostgreSQL internals. Structural records mirror
`nbtree` (`src/include/access/nbtxlog.h`, replay in `nbtxlog.c::btree_redo`):

- `SPLIT_L`/`SPLIT_R` → `LeafSplit`/`InternalSplit`
- `INSERT_UPPER` → `InsertDownlink`
- `NEWROOT` → `NewRoot`
- `VACUUM`/`DELETE` → `PageCompact`
- `MARK_PAGE_HALFDEAD`/`UNLINK_PAGE` → `MarkHalfDead`/`UnlinkPage`
- `REUSE_PAGE` → page recycle

Transaction markers are a *separate* resource manager in PostgreSQL (`XLOG_XACT_COMMIT`
/`_ABORT`, `access/xact.h`); checkpoints `access/xlog.h`. FluxDB's MVCC `Insert`/
`SetXmax` are conceptually the **heap** records (`XLOG_HEAP_INSERT`/`_DELETE`) folded
onto a B+Tree leaf because FluxDB is index-organized. FluxDB does **not** adopt the
`DEDUP` / `INSERT_POST` / `META_CLEANUP` machinery.

FluxDB's LSN is currently a **logical counter**, where PostgreSQL's LSN *is* the byte
offset into the WAL stream (`pg_lsn`) — see the reconsideration note in §10.

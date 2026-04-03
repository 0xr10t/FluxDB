//! Leaf (data) page types for the B+Tree storage engine.
//!
//! ## Layout
//!
//! ```text
//! Page size: 4096 bytes
//!
//! ┌─────────────────────────────────────────────────────────┐
//! │ FIXED HEADER — 40 bytes (fully 8-byte aligned)          │
//! ├────────┬───────────┬─────────────────────────────────── ┤
//! │ Off  0 │ u8        │ page_type                          │
//! │ Off  1 │ u8        │ flags                              │
//! │ Off  2 │ u16       │ slot_count                         │
//! │ Off  4 │ u16       │ free_start  (end of slot directory)│
//! │ Off  6 │ u16       │ free_end    (start of record area) │
//! ├────────┼───────────┼─────────────────────────────────── ┤
//! │ Off  8 │ u64       │ page_id          [8-byte aligned]  │
//! │ Off 16 │ u64       │ lsn              [8-byte aligned]  │
//! │ Off 24 │ u64       │ prev_page        [8-byte aligned]  │
//! │ Off 32 │ u64       │ next_page        [8-byte aligned]  │
//! └────────┴───────────┴─────────────────────────────────── ┘
//!
//! ┌──────────────────────────────────────────────────────────┐
//! │ SLOT DIRECTORY  [slot_count × 4 bytes, grows →]          │
//! │  slot[i] = (offset: u16, rec_size: u16)                  │
//! │  slot[i] byte offset = 40 + i*4                          │
//! └──────────────────────────────────────────────────────────┘
//!
//!          ↕  free space (free_end − free_start bytes)
//!
//! ┌──────────────────────────────────────────────────────────┐
//! │ RECORD DATA AREA  [grows ←]                              │
//! │  ┌──────┬──────┬───────┬──────┬───────────┬──────────┐  │
//! │  │ u16  │ u16  │  u8   │ u8   │ key bytes │ val bytes│  │
//! │  │k_len │v_len │ flags │ pad  │           │          │  │
//! │  └──────┴──────┴───────┴──────┴───────────┴──────────┘  │
//! │    2B     2B     1B     1B      k_len B     v_len B      │
//! │  Fixed record header = 6 bytes, padded to 8 bytes total  │
//! └──────────────────────────────────────────────────────────┘
//! ```

use std::cmp::Ordering;
use std::marker::PhantomData;
use common::{Key, Value};

use super::{
    read_u16, read_u8,
    write_u8, write_u16, write_u64,
    Lsn, PageError, PageId, PAGE_SIZE,
    OFF_LSN, OFF_PAGE_ID, OFF_PAGE_TYPE, LEAF,
};

// ── Leaf-page-specific header offsets ────────────────────────────────────────

const OFF_LEAF_SLOT_COUNT: usize = 2;  // u16
const OFF_LEAF_FREE_START: usize = 4;  // u16
const OFF_LEAF_FREE_END:   usize = 6;  // u16
// OFF_PAGE_ID at 8, OFF_LSN at 16 (shared)
const OFF_LEAF_PREV:       usize = 24; // u64
const OFF_LEAF_NEXT:       usize = 32; // u64
const LEAF_HEADER_SIZE:    usize = 40;

const SLOT_SIZE: usize = 4; // u16 offset + u16 rec_size

// ── Record layout offsets (relative to record base) ──────────────────────────

const REC_OFF_KEY_LEN: usize = 0; // u16
const REC_OFF_VAL_LEN: usize = 2; // u16
const REC_OFF_FLAGS:   usize = 4; // u8
// byte 5: padding
const REC_HEADER_SIZE: usize = 8; // padded to 8 bytes for cache alignment

// ── Record layout helpers ─────────────────────────────────────────────────────

#[inline]
fn slot_offset(i: usize) -> usize {
    LEAF_HEADER_SIZE + i * SLOT_SIZE
}

#[inline]
fn rec_key_offset(rec_base: usize) -> usize {
    rec_base + REC_HEADER_SIZE
}

/// Val start is offset by key_len rounded up to the next even byte.
#[inline]
fn rec_val_offset(rec_base: usize, key_len: usize) -> usize {
    rec_base + REC_HEADER_SIZE + ((key_len + 1) & !1)
}

/// Total record size rounded up to the next even byte so the next record stays
/// 2-byte aligned.
#[inline]
fn rec_total_size(key_len: usize, val_len: usize) -> usize {
    let raw = REC_HEADER_SIZE + ((key_len + 1) & !1) + val_len;
    (raw + 1) & !1
}

// ── LeafPageAccessor ──────────────────────────────────────────────────────────

/// Read-only typed view over a raw leaf page buffer.
///
/// The lifetime `'a` is tied to the underlying page data, not to `&self`,
/// so `get_key` / `get_value` can return zero-copy borrows that outlive the
/// accessor itself.
pub struct LeafPageAccessor<'a, K: Key, V: Value> {
    data: &'a [u8],
    _key: PhantomData<K>,
    _val: PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageAccessor<'a, K, V> {
    /// Wraps a raw page buffer.
    ///
    /// # Panics
    /// Panics if the page-type byte does not equal [`LEAF`].
    pub fn new(data: &'a [u8]) -> Self {
        assert_eq!(
            read_u8(data, OFF_PAGE_TYPE), LEAF,
            "LeafPageAccessor: page type byte is not LEAF"
        );
        Self { data, _key: PhantomData, _val: PhantomData }
    }

    // ── Page-level metadata ───────────────────────────────────────────────────

    pub fn page_id(&self) -> PageId {
        super::read_u64(self.data, OFF_PAGE_ID)
    }

    pub fn lsn(&self) -> Lsn {
        super::read_u64(self.data, OFF_LSN)
    }

    pub fn num_pairs(&self) -> u16 {
        read_u16(self.data, OFF_LEAF_SLOT_COUNT)
    }

    /// Previous leaf in the doubly-linked leaf chain, or `None` if this is the
    /// leftmost leaf.
    pub fn prev_page(&self) -> Option<PageId> {
        match super::read_u64(self.data, OFF_LEAF_PREV) {
            0 => None,
            v => Some(v),
        }
    }

    /// Next leaf in the chain, or `None` if this is the rightmost leaf.
    pub fn next_page(&self) -> Option<PageId> {
        match super::read_u64(self.data, OFF_LEAF_NEXT) {
            0 => None,
            v => Some(v),
        }
    }

    // ── Free-space accounting ─────────────────────────────────────────────────

    /// Byte offset of the first byte past the slot directory.
    pub fn free_start(&self) -> usize {
        read_u16(self.data, OFF_LEAF_FREE_START) as usize
    }

    /// Byte offset of the lowest live record (top of the record area).
    pub fn free_end(&self) -> usize {
        read_u16(self.data, OFF_LEAF_FREE_END) as usize
    }

    /// Contiguous free bytes between the slot directory and the record area.
    pub fn free_space(&self) -> usize {
        self.free_end() - self.free_start()
    }

    /// `true` if `(key_len, val_len)` fits in the current contiguous free gap
    /// without needing a `compact()`.
    pub fn can_fit_direct(&self, key_len: usize, val_len: usize) -> bool {
        self.free_space() >= SLOT_SIZE + rec_total_size(key_len, val_len)
    }

    /// `true` if the pair fits after compacting all dead record fragments.
    ///
    /// This is O(n) because it sums live record sizes. Only call it when
    /// `can_fit_direct` returns `false`.
    pub fn can_fit_after_compact(&self, key_len: usize, val_len: usize) -> bool {
        let n = self.num_pairs() as usize;
        let live_bytes: usize = (0..n).map(|i| self.slot_rec_size(i)).sum();

        let needed    = SLOT_SIZE + rec_total_size(key_len, val_len);
        let available = PAGE_SIZE
            .saturating_sub(LEAF_HEADER_SIZE)
            .saturating_sub(n * SLOT_SIZE)
            .saturating_sub(live_bytes);

        available >= needed
    }

    // ── Binary search ─────────────────────────────────────────────────────────

    /// Binary-search the sorted slot directory for `query`.
    ///
    /// Returns `(index, true)` on exact match.
    /// Returns `(index, false)` when absent; `index` is the correct insertion
    /// point (first slot whose key is greater than `query`).
    pub fn position(&self, query: &K::SelfType<'_>) -> (usize, bool) {
        let mut lo: usize = 0;
        let mut hi: usize = self.num_pairs() as usize;
        let query_bytes = K::as_bytes(query);
        let query_bytes = query_bytes.as_ref();

        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            match K::compare(self.key_bytes_at(mid), query_bytes) {
                Ordering::Less    => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal   => return (mid, true),
            }
        }
        (lo, false)
    }

    /// Returns the slot index of `query`, or `None` if absent.
    pub fn find_key(&self, query: &K::SelfType<'_>) -> Option<usize> {
        let (idx, found) = self.position(query);
        if found { Some(idx) } else { None }
    }

    // ── Record data access ────────────────────────────────────────────────────

    /// Deserialised key at slot `i`. Lifetime is `'a` (zero-copy borrow).
    pub fn get_key(&self, i: usize) -> K::SelfType<'a> {
        K::from_bytes(self.key_bytes_at(i))
    }

    /// Deserialised value at slot `i`. Lifetime is `'a` (zero-copy borrow).
    pub fn get_value(&self, i: usize) -> V::SelfType<'a> {
        V::from_bytes(self.value_bytes_at(i))
    }

    /// Both key and value at slot `i` as a tuple.
    pub fn entry(&self, i: usize) -> (K::SelfType<'a>, V::SelfType<'a>) {
        (self.get_key(i), self.get_value(i))
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    fn slot_rec_base(&self, i: usize) -> usize {
        read_u16(self.data, slot_offset(i)) as usize
    }

    fn slot_rec_size(&self, i: usize) -> usize {
        read_u16(self.data, slot_offset(i) + 2) as usize
    }

    fn key_bytes_at(&self, i: usize) -> &'a [u8] {
        let rec_base = self.slot_rec_base(i);
        let key_len  = read_u16(self.data, rec_base + REC_OFF_KEY_LEN) as usize;
        let key_off  = rec_key_offset(rec_base);
        &self.data[key_off..key_off + key_len]
    }

    fn value_bytes_at(&self, i: usize) -> &'a [u8] {
        let rec_base = self.slot_rec_base(i);
        let key_len  = read_u16(self.data, rec_base + REC_OFF_KEY_LEN) as usize;
        let val_len  = read_u16(self.data, rec_base + REC_OFF_VAL_LEN) as usize;
        let val_off  = rec_val_offset(rec_base, key_len);
        &self.data[val_off..val_off + val_len]
    }
}

// ── LeafPageMutator ───────────────────────────────────────────────────────────

/// Mutable typed view over a raw leaf page. All structural mutations go here.
pub struct LeafPageMutator<'a, K: Key, V: Value> {
    data: &'a mut [u8],
    _key: PhantomData<K>,
    _val: PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageMutator<'a, K, V> {
    /// # Panics
    /// Panics if the page-type byte does not equal [`LEAF`].
    pub fn new(data: &'a mut [u8]) -> Self {
        assert_eq!(
            read_u8(data, OFF_PAGE_TYPE), LEAF,
            "LeafPageMutator: page type byte is not LEAF"
        );
        Self { data, _key: PhantomData, _val: PhantomData }
    }

    // ── Header setters ────────────────────────────────────────────────────────

    pub fn set_lsn(&mut self, lsn: Lsn) {
        write_u64(self.data, OFF_LSN, lsn);
    }

    pub fn set_prev_page(&mut self, prev: Option<PageId>) {
        write_u64(self.data, OFF_LEAF_PREV, prev.unwrap_or(0));
    }

    pub fn set_next_page(&mut self, next: Option<PageId>) {
        write_u64(self.data, OFF_LEAF_NEXT, next.unwrap_or(0));
    }

    /// Borrow as a read-only accessor without releasing the mutable borrow.
    pub fn as_accessor(&self) -> LeafPageAccessor<'_, K, V> {
        LeafPageAccessor::new(self.data)
    }

    // ── Insert ────────────────────────────────────────────────────────────────

    /// Insert a new `(key, value)` pair at slot position `pos`, shifting all
    /// existing slots at `[pos..]` one position to the right.
    ///
    /// `pos` must be the insertion point returned by `position()` — the caller
    /// is responsible for maintaining the sorted invariant.
    ///
    /// Returns `Err(InsufficientSpace)` when the contiguous free gap is too
    /// small; the caller should `compact()` and retry if
    /// `can_fit_after_compact` returns `true`.
    pub fn insert(
        &mut self,
        pos:   usize,
        key:   &K::SelfType<'_>,
        value: &V::SelfType<'_>,
    ) -> Result<(), PageError> {
        let key_bytes = K::as_bytes(key);
        let key_bytes = key_bytes.as_ref();
        let val_bytes = V::as_bytes(value);
        let val_bytes = val_bytes.as_ref();
        let key_len  = key_bytes.len();
        let val_len  = val_bytes.len();
        let rec_size = rec_total_size(key_len, val_len);

        let free_end   = read_u16(self.data, OFF_LEAF_FREE_END)   as usize;
        let free_start = read_u16(self.data, OFF_LEAF_FREE_START) as usize;
        let free       = free_end - free_start;

        if free < SLOT_SIZE + rec_size {
            return Err(PageError::InsufficientSpace {
                needed:    SLOT_SIZE + rec_size,
                available: free,
            });
        }

        // Write the record just below free_end (record area grows downward).
        let rec_base = free_end - rec_size;

        write_u16(self.data, rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, rec_base + REC_OFF_VAL_LEN, val_len as u16);
        write_u8 (self.data, rec_base + REC_OFF_FLAGS,   0);

        let key_off = rec_key_offset(rec_base);
        self.data[key_off..key_off + key_len].copy_from_slice(key_bytes);

        let val_off = rec_val_offset(rec_base, key_len);
        self.data[val_off..val_off + val_len].copy_from_slice(val_bytes);

        // Shift slot directory: [pos..n] → [pos+1..n+1].
        // copy_within handles overlap correctly because dst > src.
        let n   = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        let src = slot_offset(pos);
        let len = (n - pos) * SLOT_SIZE;
        if len > 0 {
            self.data.copy_within(src..src + len, src + SLOT_SIZE);
        }

        // Write the new slot entry.
        write_u16(self.data, slot_offset(pos),     rec_base as u16);
        write_u16(self.data, slot_offset(pos) + 2, rec_size as u16);

        // Update header.
        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n + 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (free_start + SLOT_SIZE) as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    rec_base as u16);

        Ok(())
    }

    // ── Overwrite value ───────────────────────────────────────────────────────

    /// Replace the value stored at slot `pos`.
    ///
    /// **Same-size fast path** — bytes are written directly into the existing
    /// record. No slot entry or free-space pointer is touched.
    ///
    /// **Different-size slow path** — a fresh record is allocated in the free
    /// gap; the slot is updated to point at it; the old record becomes dead
    /// space reclaimed by `compact()`.
    ///
    /// Returns `Err(InsufficientSpace)` when the free gap is too small.
    pub fn overwrite_value(
        &mut self,
        pos:       usize,
        new_value: &V::SelfType<'_>,
    ) -> Result<(), PageError> {
        let val_bytes   = V::as_bytes(new_value);
        let val_bytes   = val_bytes.as_ref();
        let new_val_len = val_bytes.len();

        let rec_base    = read_u16(self.data, slot_offset(pos))           as usize;
        let key_len     = read_u16(self.data, rec_base + REC_OFF_KEY_LEN) as usize;
        let old_val_len = read_u16(self.data, rec_base + REC_OFF_VAL_LEN) as usize;

        if new_val_len == old_val_len {
            // Fast path: in-place write, no structural change.
            let val_off = rec_val_offset(rec_base, key_len);
            self.data[val_off..val_off + new_val_len].copy_from_slice(val_bytes);
            return Ok(());
        }

        // Slow path: allocate a new record.
        let new_rec_size = rec_total_size(key_len, new_val_len);
        let free_end     = read_u16(self.data, OFF_LEAF_FREE_END)   as usize;
        let free_start   = read_u16(self.data, OFF_LEAF_FREE_START) as usize;
        let free         = free_end - free_start;

        if free < new_rec_size {
            return Err(PageError::InsufficientSpace {
                needed:    new_rec_size,
                available: free,
            });
        }

        // Copy the key bytes out before writing, in case regions overlap.
        let old_key_off  = rec_key_offset(rec_base);
        let new_rec_base = free_end - new_rec_size;

        write_u16(self.data, new_rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, new_rec_base + REC_OFF_VAL_LEN, new_val_len as u16);
        write_u8 (self.data, new_rec_base + REC_OFF_FLAGS,   0);

        let new_key_off = rec_key_offset(new_rec_base);
        self.data.copy_within(old_key_off..old_key_off + key_len, new_key_off);

        let new_val_off = rec_val_offset(new_rec_base, key_len);
        self.data[new_val_off..new_val_off + new_val_len].copy_from_slice(val_bytes);

        // Slot now points at the new record; old record bytes are dead space.
        write_u16(self.data, slot_offset(pos),     new_rec_base as u16);
        write_u16(self.data, slot_offset(pos) + 2, new_rec_size as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    new_rec_base as u16);

        Ok(())
    }

    // ── Remove ────────────────────────────────────────────────────────────────

    /// Remove the entry at slot `pos`.
    ///
    /// The slot directory is immediately compacted: slots `[pos+1..n]` are
    /// shifted left. The record bytes in the data area are **not** erased —
    /// they become dead space. Call `compact()` to reclaim them.
    ///
    /// # Panics
    /// Panics if `pos >= num_pairs()`.
    pub fn remove(&mut self, pos: usize) {
        let n          = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        let free_start = read_u16(self.data, OFF_LEAF_FREE_START) as usize;
        assert!(pos < n, "LeafPageMutator::remove: pos {} out of bounds (n={})", pos, n);

        // Shift [pos+1..n] one slot to the left.
        let src = slot_offset(pos + 1);
        let len = (n - pos - 1) * SLOT_SIZE;
        if len > 0 {
            self.data.copy_within(src..src + len, src - SLOT_SIZE);
        }

        // Zero the vacated slot at the end of the directory.
        let vacated = slot_offset(n - 1);
        self.data[vacated..vacated + SLOT_SIZE].fill(0);

        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n - 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (free_start - SLOT_SIZE) as u16);
        // free_end is NOT updated — dead record bytes remain until compact().
    }

    // ── Compact ───────────────────────────────────────────────────────────────

    /// Reclaim dead record space left by overwrites and removals.
    ///
    /// All live records are repacked from `PAGE_SIZE` downward with no gaps,
    /// and the corresponding slot offsets are updated. After `compact()`:
    ///   `free_end = PAGE_SIZE − Σ(live record sizes)`
    ///
    /// **Algorithm**: collect live records sorted by current offset descending
    /// (closest to `PAGE_SIZE` first). Walk through them maintaining a
    /// `write_end` cursor that starts at `PAGE_SIZE` and decreases. For each
    /// record, set `write_end -= size` and copy the record there if it isn't
    /// already at that position.
    ///
    /// `copy_within` handles overlapping regions safely by copying from the
    /// high end first when `write_end > old_base`.
    pub fn compact(&mut self) {
        let n = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        if n == 0 {
            write_u16(self.data, OFF_LEAF_FREE_END, PAGE_SIZE as u16);
            return;
        }

        // Collect (slot_index, current_rec_base, rec_size) for all live slots.
        let mut live: Vec<(usize, usize, usize)> = (0..n)
            .map(|i| {
                let base = read_u16(self.data, slot_offset(i))     as usize;
                let size = read_u16(self.data, slot_offset(i) + 2) as usize;
                (i, base, size)
            })
            .collect();

        // Process the record closest to PAGE_SIZE first so every destination
        // is clear when we write to it (nothing above has been moved yet).
        live.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        let mut write_end = PAGE_SIZE;
        for (slot_idx, old_base, size) in live {
            write_end -= size;
            if old_base != write_end {
                self.data.copy_within(old_base..old_base + size, write_end);
                write_u16(self.data, slot_offset(slot_idx), write_end as u16);
            }
        }

        write_u16(self.data, OFF_LEAF_FREE_END, write_end as u16);
    }
}

// ── LeafPageBuilder ───────────────────────────────────────────────────────────
//
// Write-once constructor for a fresh leaf page. The caller pushes entries in
// ascending key order. Because entries arrive sorted, each push is O(1) —
// append a slot, write the record — no search or shift needed.

/// Write-once constructor for a fresh leaf page.
pub struct LeafPageBuilder<'a, K: Key, V: Value> {
    data:      &'a mut [u8],
    write_end: usize,
    _key:      PhantomData<K>,
    _val:      PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageBuilder<'a, K, V> {
    /// Zero the buffer, stamp the page type and page ID, initialise free
    /// pointers.
    pub fn new(page_id: PageId, data: &'a mut [u8]) -> Self {
        data.fill(0);
        write_u8 (data, OFF_PAGE_TYPE,       LEAF);
        write_u64(data, OFF_PAGE_ID,         page_id);
        write_u16(data, OFF_LEAF_FREE_START, LEAF_HEADER_SIZE as u16);
        write_u16(data, OFF_LEAF_FREE_END,   PAGE_SIZE as u16);
        Self { data, write_end: PAGE_SIZE, _key: PhantomData, _val: PhantomData }
    }

    pub fn set_prev_page(&mut self, prev: Option<PageId>) {
        write_u64(self.data, OFF_LEAF_PREV, prev.unwrap_or(0));
    }

    pub fn set_next_page(&mut self, next: Option<PageId>) {
        write_u64(self.data, OFF_LEAF_NEXT, next.unwrap_or(0));
    }

    /// Append a key-value pair. Keys must be pushed in strictly ascending
    /// order.
    ///
    /// # Panics
    /// Panics if the page has no remaining space for the record.
    pub fn push(&mut self, key: &K::SelfType<'_>, value: &V::SelfType<'_>) {
        let key_bytes = K::as_bytes(key);
        let key_bytes = key_bytes.as_ref();
        let val_bytes = V::as_bytes(value);
        let val_bytes = val_bytes.as_ref();
        let key_len  = key_bytes.len();
        let val_len  = val_bytes.len();
        let rec_size = rec_total_size(key_len, val_len);

        let n          = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        let free_start = LEAF_HEADER_SIZE + n * SLOT_SIZE;

        assert!(
            self.write_end >= free_start + SLOT_SIZE + rec_size,
            "LeafPageBuilder::push: page is full"
        );

        // Write record at the top of the free area (growing downward).
        self.write_end -= rec_size;
        let rec_base = self.write_end;

        write_u16(self.data, rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, rec_base + REC_OFF_VAL_LEN, val_len as u16);
        write_u8 (self.data, rec_base + REC_OFF_FLAGS,   0);

        let key_off = rec_key_offset(rec_base);
        self.data[key_off..key_off + key_len].copy_from_slice(key_bytes);

        let val_off = rec_val_offset(rec_base, key_len);
        self.data[val_off..val_off + val_len].copy_from_slice(val_bytes);

        // Append slot.
        let slot_off = slot_offset(n);
        write_u16(self.data, slot_off,     rec_base as u16);
        write_u16(self.data, slot_off + 2, rec_size as u16);

        // Update header.
        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n + 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (slot_off + SLOT_SIZE) as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    rec_base as u16);
    }

    /// Seal the page. Returns a [`LeafPageMutator`] for any remaining header
    /// writes (lsn, prev/next pointers) before the page is handed to the
    /// buffer pool.
    pub fn finish(self) -> LeafPageMutator<'a, K, V> {
        LeafPageMutator::new(self.data)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageBuffer;

    type K = &'static [u8];
    type V = &'static [u8];

    /// Build a leaf page from a slice of `(&[u8], &[u8])` pairs using the
    /// builder (entries must be in ascending order).
    fn build_page(page_id: u64, pairs: &[(&[u8], &[u8])]) -> PageBuffer {
        let mut buf = PageBuffer::new();
        let mut b = LeafPageBuilder::<K, V>::new(page_id, buf.memory_mut());
        for (k, v) in pairs {
            b.push(k, v);
        }
        b.finish();
        buf
    }

    // ── Builder / accessor round-trip ─────────────────────────────────────────

    #[test]
    fn build_and_read_entries() {
        let pairs: &[(&[u8], &[u8])] = &[
            (b"apple",  b"AAA"),
            (b"banana", b"BBB"),
            (b"cherry", b"CCC"),
        ];
        let buf = build_page(42, pairs);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());

        assert_eq!(acc.page_id(), 42);
        assert_eq!(acc.num_pairs(), 3);
        assert_eq!(acc.get_key(0), b"apple".as_ref());
        assert_eq!(acc.get_value(0), b"AAA".as_ref());
        assert_eq!(acc.get_key(2), b"cherry".as_ref());
        assert_eq!(acc.get_value(2), b"CCC".as_ref());
    }

    #[test]
    fn entry_returns_key_and_value_tuple() {
        let buf = build_page(1, &[(b"k1", b"v1"), (b"k2", b"v2")]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.entry(1), (b"k2".as_ref(), b"v2".as_ref()));
    }

    // ── Linked-list pointers ──────────────────────────────────────────────────

    #[test]
    fn prev_next_page_none_when_zero() {
        let buf = build_page(1, &[]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.prev_page(), None);
        assert_eq!(acc.next_page(), None);
    }

    #[test]
    fn set_prev_next_round_trip() {
        let mut buf = build_page(1, &[]);
        {
            let mut m = LeafPageMutator::<K, V>::new(buf.memory_mut());
            m.set_prev_page(Some(10));
            m.set_next_page(Some(20));
        }
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.prev_page(), Some(10));
        assert_eq!(acc.next_page(), Some(20));
    }

    #[test]
    fn set_prev_next_to_none() {
        let mut buf = build_page(1, &[]);
        {
            let mut m = LeafPageMutator::<K, V>::new(buf.memory_mut());
            m.set_prev_page(Some(5));
            m.set_prev_page(None);
        }
        assert_eq!(LeafPageAccessor::<K, V>::new(buf.memory()).prev_page(), None);
    }

    // ── LSN ───────────────────────────────────────────────────────────────────

    #[test]
    fn lsn_round_trip() {
        let mut buf = build_page(1, &[]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).set_lsn(12345);
        assert_eq!(LeafPageAccessor::<K, V>::new(buf.memory()).lsn(), 12345);
    }

    // ── Binary search (position / find_key) ──────────────────────────────────

    #[test]
    fn position_finds_exact_match() {
        let buf = build_page(1, &[(b"a", b"1"), (b"b", b"2"), (b"c", b"3")]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.position(&b"b".as_ref()), (1, true));
    }

    #[test]
    fn position_returns_insertion_point_when_absent() {
        let buf = build_page(1, &[(b"a", b"1"), (b"c", b"3")]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        // "b" is between index 0 and 1 → insertion point is 1
        assert_eq!(acc.position(&b"b".as_ref()), (1, false));
    }

    #[test]
    fn find_key_some_and_none() {
        let buf = build_page(1, &[(b"x", b"X"), (b"y", b"Y")]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.find_key(&b"x".as_ref()), Some(0));
        assert_eq!(acc.find_key(&b"z".as_ref()), None);
    }

    // ── Insert ────────────────────────────────────────────────────────────────

    #[test]
    fn insert_maintains_sorted_order() {
        let mut buf = build_page(1, &[(b"a", b"A"), (b"c", b"C")]);
        {
            let mut m = LeafPageMutator::<K, V>::new(buf.memory_mut());
            let pos = m.as_accessor().position(&b"b".as_ref()).0;
            m.insert(pos, &b"b".as_ref(), &b"B".as_ref()).unwrap();
        }
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 3);
        assert_eq!(acc.get_key(1), b"b".as_ref());
        assert_eq!(acc.get_value(1), b"B".as_ref());
        // surrounding entries must still be correct
        assert_eq!(acc.get_key(0), b"a".as_ref());
        assert_eq!(acc.get_key(2), b"c".as_ref());
    }

    #[test]
    fn insert_at_beginning() {
        let mut buf = build_page(1, &[(b"b", b"B"), (b"c", b"C")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .insert(0, &b"a".as_ref(), &b"A".as_ref())
            .unwrap();
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.get_key(0), b"a".as_ref());
        assert_eq!(acc.num_pairs(), 3);
    }

    #[test]
    fn insert_at_end() {
        let mut buf = build_page(1, &[(b"a", b"A"), (b"b", b"B")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .insert(2, &b"c".as_ref(), &b"C".as_ref())
            .unwrap();
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.get_key(2), b"c".as_ref());
        assert_eq!(acc.num_pairs(), 3);
    }

    #[test]
    fn insert_returns_err_when_no_space() {
        // Fill the page until it cannot hold a 200-byte key + 200-byte value.
        let mut buf = PageBuffer::new();
        LeafPageBuilder::<K, V>::new(1, buf.memory_mut()).finish();
        let big_key = vec![0u8; 100];
        let big_val = vec![0u8; 100];
        loop {
            let mut m = LeafPageMutator::<K, V>::new(buf.memory_mut());
            if m.as_accessor().can_fit_direct(big_key.len(), big_val.len()) {
                let n = m.as_accessor().num_pairs() as usize;
                m.insert(n, &big_key.as_slice(), &big_val.as_slice()).unwrap();
            } else {
                let result = m.insert(0, &big_key.as_slice(), &big_val.as_slice());
                assert!(matches!(result, Err(PageError::InsufficientSpace { .. })));
                break;
            }
        }
    }

    // ── Remove ────────────────────────────────────────────────────────────────

    #[test]
    fn remove_middle_entry() {
        let mut buf = build_page(1, &[(b"a", b"A"), (b"b", b"B"), (b"c", b"C")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(1);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 2);
        assert_eq!(acc.get_key(0), b"a".as_ref());
        assert_eq!(acc.get_key(1), b"c".as_ref());
    }

    #[test]
    fn remove_first_entry() {
        let mut buf = build_page(1, &[(b"a", b"A"), (b"b", b"B")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(0);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 1);
        assert_eq!(acc.get_key(0), b"b".as_ref());
    }

    #[test]
    fn remove_last_entry() {
        let mut buf = build_page(1, &[(b"a", b"A"), (b"b", b"B")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(1);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 1);
        assert_eq!(acc.get_key(0), b"a".as_ref());
    }

    #[test]
    #[should_panic(expected = "out of bounds")]
    fn remove_out_of_bounds_panics() {
        let mut buf = build_page(1, &[(b"a", b"A")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(1);
    }

    // ── Overwrite value ───────────────────────────────────────────────────────

    #[test]
    fn overwrite_same_size_fast_path() {
        let mut buf = build_page(1, &[(b"key", b"old")]);
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .overwrite_value(0, &b"new".as_ref())
            .unwrap();
        assert_eq!(
            LeafPageAccessor::<K, V>::new(buf.memory()).get_value(0),
            b"new".as_ref()
        );
    }

    #[test]
    fn overwrite_different_size_allocates_new_record() {
        let mut buf = build_page(1, &[(b"key", b"v")]);
        let free_before = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .overwrite_value(0, &b"value_longer".as_ref())
            .unwrap();
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.get_value(0), b"value_longer".as_ref());
        // free_end moved so free space decreased
        assert!(acc.free_space() < free_before);
    }

    #[test]
    fn overwrite_same_size_does_not_change_free_space() {
        let mut buf = build_page(1, &[(b"k", b"abc")]);
        let free_before = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .overwrite_value(0, &b"xyz".as_ref())
            .unwrap();
        assert_eq!(
            LeafPageAccessor::<K, V>::new(buf.memory()).free_space(),
            free_before
        );
    }

    // ── Compact ───────────────────────────────────────────────────────────────

    #[test]
    fn compact_reclaims_space_after_overwrite() {
        let mut buf = build_page(1, &[(b"key", b"short")]);
        // Overwrite with a longer value → old record becomes dead space.
        LeafPageMutator::<K, V>::new(buf.memory_mut())
            .overwrite_value(0, &b"much_longer_value".as_ref())
            .unwrap();
        let free_before_compact = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        LeafPageMutator::<K, V>::new(buf.memory_mut()).compact();
        let free_after_compact = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        // Compacting must recover some space.
        assert!(free_after_compact > free_before_compact);
        // Data integrity: key and value must still be correct.
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.get_key(0), b"key".as_ref());
        assert_eq!(acc.get_value(0), b"much_longer_value".as_ref());
    }

    #[test]
    fn compact_after_remove_reclaims_space() {
        let mut buf = build_page(1, &[
            (b"a", b"AAAA"),
            (b"b", b"BBBB"),
            (b"c", b"CCCC"),
        ]);
        // Remove the middle entry — its record bytes become dead space.
        LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(1);
        let free_before = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        LeafPageMutator::<K, V>::new(buf.memory_mut()).compact();
        let free_after = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        assert!(free_after > free_before);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 2);
        assert_eq!(acc.get_key(0), b"a".as_ref());
        assert_eq!(acc.get_key(1), b"c".as_ref());
    }

    #[test]
    fn compact_empty_page_is_safe() {
        let mut buf = build_page(1, &[]);
        LeafPageMutator::<K, V>::new(buf.memory_mut()).compact();
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert_eq!(acc.num_pairs(), 0);
        assert_eq!(acc.free_end(), PAGE_SIZE);
    }

    #[test]
    fn compact_idempotent_when_no_dead_space() {
        let mut buf = build_page(1, &[(b"k", b"v")]);
        let free_before = LeafPageAccessor::<K, V>::new(buf.memory()).free_space();
        LeafPageMutator::<K, V>::new(buf.memory_mut()).compact();
        assert_eq!(LeafPageAccessor::<K, V>::new(buf.memory()).free_space(), free_before);
    }

    // ── Free-space accounting ─────────────────────────────────────────────────

    #[test]
    fn can_fit_direct_true_on_empty_page() {
        let buf = build_page(1, &[]);
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert!(acc.can_fit_direct(10, 10));
    }

    #[test]
    fn can_fit_after_compact_true_after_removes() {
        let big_key = vec![0u8; 100];
        let big_val = vec![0u8; 100];
        let mut buf = PageBuffer::new();
        LeafPageBuilder::<K, V>::new(1, buf.memory_mut()).finish();

        // Fill the page.
        loop {
            let mut m = LeafPageMutator::<K, V>::new(buf.memory_mut());
            if m.as_accessor().can_fit_direct(big_key.len(), big_val.len()) {
                let n = m.as_accessor().num_pairs() as usize;
                m.insert(n, &big_key.as_slice(), &big_val.as_slice()).unwrap();
            } else {
                break;
            }
        }
        // Remove all but one entry to create dead space.
        let n = LeafPageAccessor::<K, V>::new(buf.memory()).num_pairs();
        for _ in 1..n {
            LeafPageMutator::<K, V>::new(buf.memory_mut()).remove(1);
        }
        // Direct fit won't work (dead space, no contiguous gap), but after
        // compact it should.
        let acc = LeafPageAccessor::<K, V>::new(buf.memory());
        assert!(acc.can_fit_after_compact(big_key.len(), big_val.len()));
    }
}

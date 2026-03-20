use std::cmp::Ordering;
use std::marker::PhantomData;
use common::{Key, Value};

pub type PageId = u64;
pub type Lsn    = u64;
pub type SlotId = u16;

pub const LEAF: u8 = 1; // byte = 1 representing leaf page
pub const INTERNAL: u8 = 2; // byte =2 representing internal page 

// A standard page size is 4096 bytes (4KB)
pub const PAGE_SIZE: usize = 4096;
// ── Shared ────────────────────────────────────────────
/* 
┌─────────────────────────────────────────────────────────┐
│ FIXED HEADER — 32 bytes (fully 8-byte aligned)          │
├────────┬───────────┬──────────────────────────────────  ┤
│ Off  0 │ u8        │ page_type                           │
│ Off  1 │ u8        │ flags (dirty, root, etc.)           │
│ Off  2 │ u16       │ num_keys                            │
│ Off  4 │ u32       │ ← padding: brings header to 8 bytes │
├────────┼───────────┼───────────────────────────────────  ┤
│ Off  8 │ u64       │ page_id          [8-byte aligned ✓] │
│ Off 16 │ u64       │ lsn              [8-byte aligned ✓] │
│ Off 24 │ u64       │ parent_page_id   [8-byte aligned ✓] │
└────────┴───────────┴───────────────────────────────────  ┘

┌──────────────────────────────────────────────────────────┐
│ SECTION A — Child page IDs  [(num_keys + 1) × 8 bytes]   │
│                                                          │
│  Offset: 32                                              │
│  32 % 8 == 0 ✓                                           │
│  Each u64 entry keeps alignment since 8 % 8 == 0         │
│                                                          │
│  child[i] offset = 32 + i*8                              │
└──────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────┐
│ SECTION B — Key end offsets  [num_keys × 4 bytes]        │
│                                                          │
│  Offset: 32 + (num_keys+1)*8                             │
│                                                          │
│                                                          │
│                                                          │
│                                                          │
│  key_end[i] offset = 32 + (num_keys+1)*8 + i*4           │
│  key_end[i] stores: exclusive end of key[i] in           │
│  Section C, relative to Section C start                  │
└──────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────┐
│ SECTION C — Key data  [variable, packed bytes]           │
│                                                          │
│  Starts immediately after Section B — no padding needed  │
│  since key data is raw bytes ([u8]) with no alignment    │
│  requirement. Every access uses from_le_bytes / slices.  │
│                                                          │
│  key[i] spans [key_end[i-1], key_end[i])                 │
│  (key_end[-1] is defined as 0)                           │
└──────────────────────────────────────────────────────────┘

*/

// these are the starting byte offsets for both the page types 
const OFF_PAGE_TYPE:   usize = 0;   // u8
const OFF_FLAGS:       usize = 1;   // u8
// 2..8 differ by page type (see below)
const OFF_PAGE_ID:     usize = 8;   // u64
const OFF_LSN:         usize = 16;  // u64

// ── Internal page ─────────────────────────────────────
const OFF_INT_NUM_KEYS:     usize = 2;   // u16
// OFF_FLAGS at 1, OFF_INT_NUM_KEYS at 2: u16 at 2-byte boundary ✓
// bytes 4..8: u32 padding
const OFF_INT_PARENT:       usize = 24;  // u64
const INT_HEADER_SIZE:      usize = 32;

// calculates the offset of child 
fn int_child_offset(i: usize) -> usize {
    INT_HEADER_SIZE + i * 8
}
// calculates the start of key_end_offset section 
fn int_key_end_section(num_keys: usize) -> usize {
    INT_HEADER_SIZE + (num_keys + 1) * 8
}
// calculates the offset which stores the offset of key_end. i starts with 0 
fn int_key_end_offset(num_keys: usize, i: usize) -> usize {
    int_key_end_section(num_keys) + i * 4
}
// returns the start of offset where key data is stored.
// Section C starts immediately after Section B — no padding needed since
// key data is raw bytes ([u8]) with no alignment requirement.
fn int_key_data_base(num_keys: usize) -> usize {
    int_key_end_section(num_keys) + num_keys * 4
}

/*
Page size: 4096 bytes

┌─────────────────────────────────────────────────────────┐
│ FIXED HEADER — 40 bytes (fully 8-byte aligned)          │
├────────┬───────────┬─────────────────────────────────── ┤
│ Off  0 │ u8        │ page_type                          │
│ Off  1 │ u8        │ flags                              │
│ Off  2 │ u16       │ slot_count                         │
│ Off  4 │ u16       │ free_start  (end of slot directory)│
│ Off  6 │ u16       │ free_end    (start of record area) │
├────────┼───────────┼─────────────────────────────────── ┤
│ Off  8 │ u64       │ page_id          [8-byte aligned ] │
│ Off 16 │ u64       │ lsn              [8-byte aligned ] │
│ Off 24 │ u64       │ prev_page        [8-byte aligned ] │
│ Off 32 │ u64       │ next_page        [8-byte aligned ] │
└────────┴───────────┴─────────────────────────────────── ┘

┌──────────────────────────────────────────────────────────┐
│ SLOT DIRECTORY  [slot_count × 4 bytes, grows →]          │
│                                                          │
│  Starts at offset 40  (40 % 8 == 0 )                     │
│  Each slot entry = 4 bytes (u16 offset + u16 length)     │
│                                                          │
│                                                          │
│                                                          │
│                                                          │
│  slot[i] byte offset = 40 + i*4                          │
│  Entries are 4-byte aligned throughout                   │
└──────────────────────────────────────────────────────────┘

         ↕  free space (free_end - free_start bytes)

┌──────────────────────────────────────────────────────────┐
│ RECORD DATA AREA  [grows ←, records packed from top]     │
│                                                          │
│  Each record layout (fixed header first, data second):   │
│  ┌──────┬──────┬───────┬─────────────────────────────┐   │
│  │ u16  │ u16  │ u8    │ u8 pad │ key bytes │ val bytes│ │
│  │k_len │v_len │ flags │        │           │          │ │
│  └──────┴──────┴───────┴─────────────────────────────┘   │
│    2B     2B     1B      1B       k_len B    v_len B     │
│                                                          │
│  Fixed record header = 6 bytes, padded to 8 bytes total  │
│  Allocation always rounded up to nearest 2 bytes so      │
│  every record starts at an even offset                   │
│                                                          │
│  key_len + val_len + 8 (overhead) per record             │
└──────────────────────────────────────────────────────────┘

*/
// ── Leaf page ─────────────────────────────────────────
const OFF_LEAF_SLOT_COUNT:  usize = 2;   // u16
const OFF_LEAF_FREE_START:  usize = 4;   // u16
const OFF_LEAF_FREE_END:    usize = 6;   // u16
// OFF_PAGE_ID at 8, OFF_LSN at 16 (shared)
const OFF_LEAF_PREV:        usize = 24;  // u64
const OFF_LEAF_NEXT:        usize = 32;  // u64
const LEAF_HEADER_SIZE:     usize = 40;
const SLOT_SIZE:            usize = 4;   // u16 offset + u16 length

// Record layout within data area
const REC_OFF_KEY_LEN: usize = 0;   // u16
const REC_OFF_VAL_LEN: usize = 2;   // u16
const REC_OFF_FLAGS:   usize = 4;   // u8
// byte 5: padding
const REC_HEADER_SIZE: usize = 8;   // padded to 8 bytes (u64 cache-friendly)

#[inline]
fn slot_offset(i: usize) -> usize {
    LEAF_HEADER_SIZE + i * SLOT_SIZE
}

#[inline]
fn rec_key_offset(rec_base: usize) -> usize {
    rec_base + REC_HEADER_SIZE
}

#[inline]
fn rec_val_offset(rec_base: usize, key_len: usize) -> usize {
    // round key_len to 2-byte boundary so val starts 2-byte aligned
    rec_base + REC_HEADER_SIZE + ((key_len + 1) & !1)
}

#[inline]
fn rec_total_size(key_len: usize, val_len: usize) -> usize {
    // entire record rounded up to 2 bytes so next record stays 2-byte aligned
    let raw = REC_HEADER_SIZE + ((key_len + 1) & !1) + val_len;
    (raw + 1) & !1
}

pub struct PageBuffer {
    // We use a boxed array to ensure the memory is allocated on the heap, 
    // not the stack, to prevent stack overflows, but it's completely contiguous.
    data: Box<[u8; PAGE_SIZE]>,
}

impl PageBuffer {
    /// Creates a completely new, zeroed-out page
    pub fn new() -> Self {
        Self {
            // Box::new([0; PAGE_SIZE]) is often unoptimized in Debug mode.
            // Using vec! prevents stack blowouts before moving to the Box array.
            data: vec![0; PAGE_SIZE].into_boxed_slice().try_into().unwrap(),
        }
    }

    /// Provides raw read access to the bytes
    pub fn memory(&self) -> &[u8] {
        self.data.as_ref()
    }

    /// Provides raw write access to the bytes
    pub fn memory_mut(&mut self) -> &mut [u8] {
        self.data.as_mut()
    }
}
// helper functions to read values at offsets: 


fn read_u8(data: &[u8], off: usize) -> u8 {
    data[off]
}

fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}

fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}


fn write_u8(data: &mut [u8], off: usize, val: u8) {
    data[off] = val;
}

fn write_u16(data: &mut [u8], off: usize, val: u16) {
    data[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

fn write_u32(data: &mut [u8], off: usize, val: u32) {
    data[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

fn write_u64(data: &mut [u8], off: usize, val: u64) {
    data[off..off + 8].copy_from_slice(&val.to_le_bytes());
}


pub struct InternalPageAccessor <'a, K: Key> {
    data: &'a [u8],
    _key: PhantomData<K> 
}

impl<'a, K: Key> InternalPageAccessor<'a, K> {
    pub fn new(data: &'a [u8]) -> Self {
        debug_assert_eq!(read_u8(data, OFF_PAGE_TYPE), INTERNAL, "not an internal page");
        Self { 
            data,
            _key: PhantomData,
        }
    }

    pub fn lsn(&self) -> Lsn {
        read_u64(self.data, OFF_LSN)
    }

    pub fn page_id(&self) -> PageId {
        read_u64(self.data, OFF_PAGE_ID)
    }

    pub fn num_keys(&self) -> u16 {
        read_u16(self.data, OFF_INT_NUM_KEYS)
    }

    pub fn parent_page_id(&self) -> Option<PageId> {
        match read_u64(self.data, OFF_INT_PARENT) {
            0   => None, 
            v => Some(v),
        }
    }

    pub fn child_page_at(&self, i: usize) -> PageId {
        read_u64(self.data, int_child_offset(i))
    }

    pub fn key_at(&self, i: usize) -> K::SelfType<'a> {
        // key_bytes_at returns &'a [u8] so from_bytes produces K::SelfType<'a>
        K::from_bytes(self.key_bytes_at(i))
    }

    pub fn find_child(&self, search_key: &K::SelfType<'_>) -> (usize,PageId) {
        let n = self.num_keys(); 
        let mut high = n as usize;
        let mut low = 0usize; 
        let search_bytes = K::as_bytes(search_key);
        let search_bytes = search_bytes.as_ref();

        while low < high {
            let mid = low + (high-low)/2; 
            match K::compare(self.key_bytes_at(mid), search_bytes) {
                Ordering::Greater              => high = mid,     // key[mid] > search: answer is <= mid
                Ordering::Less | Ordering::Equal => low = mid + 1, // key[mid] <= search: answer is > mid
            }
        }
        // low = index of first key > search_key 
        //     = contains the child index to descend to. 
        (low, self.child_page_at(low))

    }

    // Returns raw bytes for key[i]. Lifetime is 'a — tied to the page data,
    // not to &self — so key_at can hand the slice straight to K::from_bytes. 
    fn key_bytes_at(&self, i: usize) -> &'a [u8] {
        let n    = self.num_keys() as usize;
        let base = int_key_data_base(n);
        let start = if i == 0 { 0 } else {
            read_u32(self.data, int_key_end_offset(n, i - 1)) as usize
        };
        let end = read_u32(self.data, int_key_end_offset(n, i)) as usize;
        &self.data[base + start..base + end]
    }

    fn used_bytes(&self) -> usize {
        let n = self.num_keys() as usize;

        // Last key_end stores cumulative total of all key data bytes
        let key_data_size = if n == 0 {
            0
        } else {
            read_u32(self.data, int_key_end_offset(n, n - 1)) as usize
        };

        INT_HEADER_SIZE
            + (n + 1) * 8   // Section A: child page IDs
            + n * 4          // Section B: key end offsets
            + key_data_size  // Section C: key data
    }

    pub fn free_bytes(&self) -> usize {
        PAGE_SIZE - self.used_bytes()
    }

    pub fn can_fit(&self, key_len: usize) -> bool {
        // Space needed for one more key+child:
        //   8 bytes  → one more child page ID (Section A)
        //   4 bytes  → one more key_end entry (Section B)
        //   key_len  → the actual key bytes (Section C)
        let needed = 8 + 4 + key_len;
        self.free_bytes() >= needed
    }

}

// ── Shared error type ──────────────────────────────────────────────────────
#[derive(Debug)]
pub enum PageError {
    InsufficientSpace { needed: usize, available: usize },
}

// Which adjacent child to keep when a key is removed during a merge.
pub enum ChildSide {
    Left,   // keep child[index],     drop child[index + 1]
    Right,  // keep child[index + 1], drop child[index]
}

// ── InternalPageMutator ───────────────────────────────────────────────────

pub struct InternalPageMutator<'a, K: Key> {
    data: &'a mut [u8],
    _key: PhantomData<K>,
}

impl<'a, K: Key> InternalPageMutator<'a, K> {
    pub fn new(data: &'a mut [u8]) -> Self {
        debug_assert_eq!(read_u8(data, OFF_PAGE_TYPE), INTERNAL, "not an internal page");
        Self { data, _key: PhantomData }
    }

    pub fn set_lsn(&mut self, lsn: Lsn) {
        write_u64(self.data, OFF_LSN, lsn);
    }

    pub fn set_parent_page_id(&mut self, parent: Option<PageId>) {
        write_u64(self.data, OFF_INT_PARENT, parent.unwrap_or(0));
    }

    // Direct child pointer update — used when a child page is replaced
    // (e.g. after a page split assigns a new page id to a child).
    pub fn set_child_at(&mut self, i: usize, page_id: PageId) {
        write_u64(self.data, int_child_offset(i), page_id);
    }

    pub fn as_accessor(&self) -> InternalPageAccessor<'_, K> {
        InternalPageAccessor::new(self.data)
    }

    // ── Insert ────────────────────────────────────────────────────────────
    // Called after a child splits. The promoted separator key is inserted at
    // `index` and the new right sibling becomes child[index + 1].
    //
    // Before:  ... | child[index] | key[index] | child[index+1] | ...
    // After:   ... | child[index] | key(new)   | right_child    | key[index] | child[index+1] | ...
    pub fn insert_key_and_right_child(
        &mut self,
        index:       usize,
        key:         &K::SelfType<'_>,
        right_child: PageId,
    ) -> Result<(), PageError> {
        let key_bytes = K::as_bytes(key);
        let key_bytes = key_bytes.as_ref();

        if !self.as_accessor().can_fit(key_bytes.len()) {
            return Err(PageError::InsufficientSpace {
                needed:    8 + 4 + key_bytes.len(),
                available: self.as_accessor().free_bytes(),
            });
        }

        let (mut children, mut keys) = self.snapshot();
        children.insert(index + 1, right_child);
        keys.insert(index, key_bytes.to_vec());
        self.rewrite(&children, &keys);
        Ok(())
    }

    // ── Remove ────────────────────────────────────────────────────────────
    // Called during a merge. Removes key[index] and one of its adjacent
    // children depending on `keep`.
    pub fn remove_key_at(&mut self, index: usize, keep: ChildSide) {
        let (mut children, mut keys) = self.snapshot();
        keys.remove(index);
        match keep {
            ChildSide::Left  => { children.remove(index + 1); } // drop right sibling
            ChildSide::Right => { children.remove(index); }     // drop left sibling
        }
        self.rewrite(&children, &keys);
    }

    // ── Private helpers ───────────────────────────────────────────────────

    // Copies all children and key bytes off the page into owned Vecs so we
    // can safely modify them before rewriting. Necessary because any change
    // to num_keys shifts every section offset.
    fn snapshot(&self) -> (Vec<PageId>, Vec<Vec<u8>>) {
        let n = read_u16(self.data, OFF_INT_NUM_KEYS) as usize;

        let children = (0..=n)
            .map(|i| read_u64(self.data, int_child_offset(i)))
            .collect();

        let base = int_key_data_base(n);
        let keys = (0..n)
            .map(|i| {
                let start = if i == 0 {
                    0
                } else {
                    read_u32(self.data, int_key_end_offset(n, i - 1)) as usize
                };
                let end = read_u32(self.data, int_key_end_offset(n, i)) as usize;
                self.data[base + start..base + end].to_vec()
            })
            .collect();

        (children, keys)
    }

    // Writes children and keys back to the page in the correct section layout.
    // num_keys is derived from keys.len().
    fn rewrite(&mut self, children: &[PageId], keys: &[Vec<u8>]) {
        let new_n = keys.len();
        write_u16(self.data, OFF_INT_NUM_KEYS, new_n as u16);

        // Section A — child page IDs
        for (i, &child) in children.iter().enumerate() {
            write_u64(self.data, int_child_offset(i), child);
        }

        // Section B — cumulative key end offsets
        let mut cumulative = 0usize;
        for (i, key) in keys.iter().enumerate() {
            cumulative += key.len();
            write_u32(self.data, int_key_end_offset(new_n, i), cumulative as u32);
        }

        // Section C — packed key data
        let base = int_key_data_base(new_n);
        let mut off = base;
        for key in keys {
            self.data[off..off + key.len()].copy_from_slice(key);
            off += key.len();
        }
    }
}

// ── InternalPageBuilder ───────────────────────────────────────────────────
// Keys are buffered in `keys` and written all at once in `finish()` because
// every section offset depends on the final num_keys.

pub struct InternalPageBuilder<'a, K: Key> {
    data:     &'a mut [u8],
    keys:     Vec<Vec<u8>>,
    children: Vec<PageId>,
    _key:     PhantomData<K>,
}

impl<'a, K: Key> InternalPageBuilder<'a, K> {
    pub fn new(page_id: PageId, data: &'a mut [u8]) -> Self {
        data.fill(0);
        write_u8 (data, OFF_PAGE_TYPE, INTERNAL);
        write_u64(data, OFF_PAGE_ID,   page_id);
        Self {
            data,
            keys:     Vec::new(),
            children: Vec::new(),
            _key:     PhantomData,
        }
    }

    // Must be called exactly once before any push_key_and_right_child calls.
    pub fn push_first_child(&mut self, child: PageId) {
        debug_assert!(self.children.is_empty(), "push_first_child must be called first");
        self.children.push(child);
    }
    // the caller is responsible to make sure keys are in ascending order 
    // Keys must be pushed in ascending order. Each call adds one separator
    // key and the right child that follows it.
    pub fn push_key_and_right_child(&mut self, key: &K::SelfType<'_>, right_child: PageId) {
        debug_assert!(!self.children.is_empty(), "call push_first_child first");
        let key_bytes = K::as_bytes(key);
        self.keys.push(key_bytes.as_ref().to_vec());
        self.children.push(right_child);
    }

    // Seals the page: writes all buffered data with the correct layout,
    // then returns a mutator for post-build header fields (lsn, parent).
    pub fn finish(self) -> InternalPageMutator<'a, K> {
        let Self { data, keys, children, .. } = self;
        let num_keys = keys.len();

        write_u16(data, OFF_INT_NUM_KEYS, num_keys as u16);

        // Section A — child page IDs
        for (i, &child) in children.iter().enumerate() {
            write_u64(data, int_child_offset(i), child);
        }

        // Section B — cumulative key end offsets
        let mut cumulative = 0usize;
        for (i, key) in keys.iter().enumerate() {
            cumulative += key.len();
            write_u32(data, int_key_end_offset(num_keys, i), cumulative as u32);
        }

        // Section C — packed key data
        let base = int_key_data_base(num_keys);
        let mut off = base;
        for key in &keys {
            data[off..off + key.len()].copy_from_slice(key);
            off += key.len();
        }

        InternalPageMutator::new(data)
    }
}

// ── LeafPageAccessor ──────────────────────────────────────────────────────
// Read-only typed view over a leaf page's raw bytes.
//
// The lifetime 'a is tied to the underlying page data, not to &self.
// That means methods like get_key / get_value can hand zero-copy borrows
// back to the caller — the returned K::SelfType<'a> / V::SelfType<'a> live
// as long as the page buffer, not just as long as the accessor.

pub struct LeafPageAccessor<'a, K: Key, V: Value> {
    data: &'a [u8],
    _key: PhantomData<K>,
    _val: PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageAccessor<'a, K, V> {
    pub fn new(data: &'a [u8]) -> Self {
        debug_assert_eq!(read_u8(data, OFF_PAGE_TYPE), LEAF, "not a leaf page");
        Self { data, _key: PhantomData, _val: PhantomData }
    }

    // ── Page-level metadata ───────────────────────────────────────────────

    pub fn page_id(&self) -> PageId {
        read_u64(self.data, OFF_PAGE_ID)
    }

    pub fn lsn(&self) -> Lsn {
        read_u64(self.data, OFF_LSN)
    }

    pub fn num_pairs(&self) -> u16 {
        read_u16(self.data, OFF_LEAF_SLOT_COUNT)
    }

    /// Previous leaf in the B-tree's doubly-linked leaf chain, or None if
    /// this is the leftmost leaf.
    pub fn prev_page(&self) -> Option<PageId> {
        match read_u64(self.data, OFF_LEAF_PREV) {
            0 => None,
            v => Some(v),
        }
    }

    /// Next leaf in the chain, or None if this is the rightmost leaf.
    pub fn next_page(&self) -> Option<PageId> {
        match read_u64(self.data, OFF_LEAF_NEXT) {
            0 => None,
            v => Some(v),
        }
    }

    // ── Free-space accounting ─────────────────────────────────────────────

    /// Byte offset of the first byte past the slot directory.
    pub fn free_start(&self) -> usize {
        read_u16(self.data, OFF_LEAF_FREE_START) as usize
    }

    /// Byte offset of the lowest live record (top of the record area).
    pub fn free_end(&self) -> usize {
        read_u16(self.data, OFF_LEAF_FREE_END) as usize
    }

    /// Contiguous free bytes between slot directory and record area.
    pub fn free_space(&self) -> usize {
        self.free_end() - self.free_start()
    }

    /// True if (key_len, val_len) fits in the current contiguous free gap
    /// without needing a compact().
    pub fn can_fit_direct(&self, key_len: usize, val_len: usize) -> bool {
        self.free_space() >= SLOT_SIZE + rec_total_size(key_len, val_len)
    }

    /// True if the pair fits after compacting all dead record fragments.
    ///
    /// This is O(n) because it has to sum the live record sizes; only call
    /// it when can_fit_direct returns false.
    /// the records may not be deleted but the slots are deleted immediately so total len by slots will give the size after compact
    pub fn can_fit_after_compact(&self, key_len: usize, val_len: usize) -> bool {
        let n = self.num_pairs() as usize;
        let live_bytes: usize = (0..n).map(|i| self.slot_rec_size(i)).sum();

        // After compact, the free gap becomes:
        //   PAGE_SIZE - LEAF_HEADER_SIZE - n*SLOT_SIZE - live_bytes
        // We need room for one more slot + the new record.
        let needed = SLOT_SIZE + rec_total_size(key_len, val_len);
        let available = PAGE_SIZE
            .saturating_sub(LEAF_HEADER_SIZE)
            .saturating_sub(n * SLOT_SIZE)
            .saturating_sub(live_bytes);

        available >= needed
    }

    // ── Binary search ─────────────────────────────────────────────────────

    /// Binary search the sorted slot directory for `query`.
    ///
    /// Returns `(index, true)` on exact match.
    /// Returns `(index, false)` when absent; `index` is the correct insertion
    /// point — the first slot whose key compares Greater than `query`.
    ///
    /// Returning both the index and the found flag in one call means callers
    /// never re-run the search: `insert` uses (pos, false) to know where to
    /// shift; `find_key` and `get_value` use (pos, true).
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

    /// Returns the slot index of `query`, or None if not present.
    pub fn find_key(&self, query: &K::SelfType<'_>) -> Option<usize> {
        let (idx, found) = self.position(query);
        if found { Some(idx) } else { None }
    }

    // ── Record data access ────────────────────────────────────────────────

    /// Deserialised key at slot `i`.
    /// Lifetime is 'a — the returned value may borrow from the page
    pub fn get_key(&self, i: usize) -> K::SelfType<'a> {
        K::from_bytes(self.key_bytes_at(i))
    }

    /// Deserialised value at slot `i`.
    pub fn get_value(&self, i: usize) -> V::SelfType<'a> {
        V::from_bytes(self.value_bytes_at(i))
    }

    /// Both key and value at slot `i` as a tuple.
    pub fn entry(&self, i: usize) -> (K::SelfType<'a>, V::SelfType<'a>) {
        (self.get_key(i), self.get_value(i))
    }

    // ── Private helpers ───────────────────────────────────────────────────

    /// gives offset of record referenced by slot at index i
    fn slot_rec_base(&self, i: usize) -> usize {
        read_u16(self.data, slot_offset(i)) as usize
    }

    fn slot_rec_size(&self, i: usize) -> usize {
        read_u16(self.data, slot_offset(i) + 2) as usize
    }

    /// Raw key bytes at slot `i`. Lifetime 'a lets get_key hand the slice
    /// directly to K::from_bytes without an extra copy.
    fn key_bytes_at(&self, i: usize) -> &'a [u8] {
        let rec_base = self.slot_rec_base(i);
        let key_len  = read_u16(self.data, rec_base + REC_OFF_KEY_LEN) as usize;
        let key_off  = rec_key_offset(rec_base);
        &self.data[key_off..key_off + key_len]
    }

    /// Raw value bytes at slot `i`.
    fn value_bytes_at(&self, i: usize) -> &'a [u8] {
        let rec_base = self.slot_rec_base(i);
        let key_len  = read_u16(self.data, rec_base + REC_OFF_KEY_LEN) as usize;
        let val_len  = read_u16(self.data, rec_base + REC_OFF_VAL_LEN) as usize;
        let val_off  = rec_val_offset(rec_base, key_len);
        &self.data[val_off..val_off + val_len]
    }
}

// ── LeafPageMutator ───────────────────────────────────────────────────────
// Mutable typed view over a leaf page. All structural mutations go here.

pub struct LeafPageMutator<'a, K: Key, V: Value> {
    data: &'a mut [u8],
    _key: PhantomData<K>,
    _val: PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageMutator<'a, K, V> {
    pub fn new(data: &'a mut [u8]) -> Self {
        debug_assert_eq!(read_u8(data, OFF_PAGE_TYPE), LEAF, "not a leaf page");
        Self { data, _key: PhantomData, _val: PhantomData }
    }

    // ── Header setters ────────────────────────────────────────────────────

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

    // ── Insert ────────────────────────────────────────────────────────────
    /// Insert a new (key, value) pair at slot position `pos`, shifting all
    /// existing slots at `[pos..]` one position to the right.
    ///
    /// `pos` must be the insertion point returned by `position()` — the
    /// caller is responsible for maintaining the sorted invariant.
    ///
    /// Returns `Err(InsufficientSpace)` when the contiguous free gap is too
    /// small. The caller should call `compact()` and retry if
    /// `can_fit_after_compact` returns true.
    ///
    /// Memory layout after insert:
    ///   slot dir:   [0 .. pos-1] [NEW] [pos .. n-1]   (grows →)
    ///   record area: new record written just below free_end (grows ←)
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

        // ── Write record (top of free gap, growing downward) ──────────────
        let rec_base = free_end - rec_size;

        write_u16(self.data, rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, rec_base + REC_OFF_VAL_LEN, val_len as u16);
        write_u8 (self.data, rec_base + REC_OFF_FLAGS,   0);

        let key_off = rec_key_offset(rec_base);
        self.data[key_off..key_off + key_len].copy_from_slice(key_bytes);

        let val_off = rec_val_offset(rec_base, key_len);
        self.data[val_off..val_off + val_len].copy_from_slice(val_bytes);

        // ── Shift slot directory: [pos..n] → [pos+1..n+1] ────────────────
        // copy_within handles the overlap correctly since dst > src.
        let n   = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        let src = slot_offset(pos);
        let len = (n - pos) * SLOT_SIZE;
        if len > 0 {
            // shift all data by 4 byte(SLOT SIZE)
            self.data.copy_within(src..src + len, src + SLOT_SIZE);
        }

        // ── Write new slot ────────────────────────────────────────────────
        // Slot layout: (rec_base: u16, rec_total_size: u16)
        // Caching rec_size in the slot avoids re-deriving it during compact().
        write_u16(self.data, slot_offset(pos),     rec_base as u16);
        write_u16(self.data, slot_offset(pos) + 2, rec_size as u16);

        // ── Update header ─────────────────────────────────────────────────
        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n + 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (free_start + SLOT_SIZE) as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    rec_base as u16);

        Ok(())
    }

    // ── Overwrite value ───────────────────────────────────────────────────
    /// Replace the value stored at slot `pos`.
    ///
    /// **Same-size fast path** — when the new value serialises to the same
    /// byte length as the existing one, the bytes are written directly into
    /// the existing record. No slot entry or free-space pointer is touched.
    /// This is a single copy into the record area and is essentially free.
    ///
    /// **Different-size slow path** — a fresh record is allocated in the free
    /// gap. The slot is updated to point at the new record; the old record's
    /// bytes become dead space that `compact()` can later reclaim.
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
            // ── Fast path: in-place write, no structural changes ──────────
            let val_off = rec_val_offset(rec_base, key_len);
            self.data[val_off..val_off + new_val_len].copy_from_slice(val_bytes);
            return Ok(());
        }

        // ── Slow path: allocate a new record ─────────────────────────────
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

        // Copy the key bytes out before writing into the page, in case the
        // old and new record regions overlap.
        let old_key_off = rec_key_offset(rec_base);
        let new_rec_base   = free_end - new_rec_size;

        write_u16(self.data, new_rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, new_rec_base + REC_OFF_VAL_LEN, new_val_len as u16);
        write_u8 (self.data, new_rec_base + REC_OFF_FLAGS,   0);

        let new_key_off = rec_key_offset(new_rec_base);
        self.data.copy_within(old_key_off..old_key_off+key_len, new_key_off);

        let new_val_off = rec_val_offset(new_rec_base, key_len);
        self.data[new_val_off..new_val_off + new_val_len].copy_from_slice(val_bytes);

        // Slot now points at the new record; old record is dead space.
        write_u16(self.data, slot_offset(pos),     new_rec_base as u16);
        write_u16(self.data, slot_offset(pos) + 2, new_rec_size as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    new_rec_base as u16);

        Ok(())
    }

    // ── Remove ────────────────────────────────────────────────────────────
    /// Remove the entry at slot `pos`.
    ///
    /// The slot directory is immediately compacted: slots `[pos+1..n]` are
    /// shifted left by one, keeping the directory dense and sorted. The
    /// record's bytes in the data area are NOT erased — they become dead
    /// space. Call `compact()` to reclaim them.
    pub fn remove(&mut self, pos: usize) {
        let n          = read_u16(self.data, OFF_LEAF_SLOT_COUNT)  as usize;
        let free_start = read_u16(self.data, OFF_LEAF_FREE_START) as usize;
        debug_assert!(pos < n, "remove: pos {} out of bounds (n={})", pos, n);

        // Shift [pos+1..n] one slot to the left.
        let src = slot_offset(pos + 1);
        let len = (n - pos - 1) * SLOT_SIZE;
        if len > 0 {
            self.data.copy_within(src..src + len, src - SLOT_SIZE);
        }

        
        let vacated = slot_offset(n - 1);
        self.data[vacated..vacated + SLOT_SIZE].fill(0);

        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n - 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (free_start - SLOT_SIZE) as u16);
        // free_end is NOT updated — dead record bytes remain until compact().
    }

    // ── Compact ───────────────────────────────────────────────────────────
    /// Reclaim dead record space left by overwrites and removals.
    ///
    /// All live records are repacked from PAGE_SIZE downward with no gaps,
    /// and the corresponding slot offsets are updated. After compact():
    ///   free_end = PAGE_SIZE − Σ(live record sizes)
    ///
    /// **Algorithm**: sort live records by current offset descending (record
    /// closest to PAGE_SIZE first). Walk through them, maintaining a
    /// `write_end` cursor that starts at PAGE_SIZE and decreases. For each
    /// record, set `write_end -= size` and copy the record there if it isn't
    /// already at that position.
    ///
    /// Correctness of overlapping copies: when `write_end > old_base`, the
    /// destination is above the source in the page (toward PAGE_SIZE).
    /// `copy_within` handles this safely by copying from the high end first.
    pub fn compact(&mut self) {
        let n = read_u16(self.data, OFF_LEAF_SLOT_COUNT) as usize;
        if n == 0 {
            write_u16(self.data, OFF_LEAF_FREE_END, PAGE_SIZE as u16);
            return;
        }

        // Collect (slot_index, current_rec_base, rec_size) for all live records.
        let mut live: Vec<(usize, usize, usize)> = (0..n)
            .map(|i| {
                let base = read_u16(self.data, slot_offset(i))     as usize;
                let size = read_u16(self.data, slot_offset(i) + 2) as usize;
                (i, base, size)
            })
            .collect();

        // Process record closest to PAGE_SIZE first so every destination is
        // clear when we write to it (nothing above has been moved yet).
        // this sorts in descending order based on offsets. largest offsets come first as records grow from high-address-end(right side). 
        live.sort_unstable_by(|a, b| b.1.cmp(&a.1));

        let mut write_end = PAGE_SIZE;
        for (slot_idx, old_base, size) in live {
            write_end -= size;
            if old_base != write_end {
                self.data.copy_within(old_base..old_base + size, write_end);
                // store the offset in slot
                write_u16(self.data, slot_offset(slot_idx), write_end as u16);
            }
        }

        write_u16(self.data, OFF_LEAF_FREE_END, write_end as u16);
    }
}

// ── LeafPageBuilder ───────────────────────────────────────────────────────
// Write-once constructor for a fresh leaf page.
//
// Used during page creation and leaf splits. The caller pushes entries in
// ascending key order. Because entries arrive sorted, each push is O(1):
// append a slot to the directory, write the record from the top of the data
// area. There is no snapshot/rewrite step and no intermediate Vec.
//
// Call finish() to seal the page and get a LeafPageMutator for any remaining
// header writes (lsn, prev/next pointers) before handing the page to the
// buffer pool.

pub struct LeafPageBuilder<'a, K: Key, V: Value> {
    data:      &'a mut [u8],
    write_end: usize,   // next record will be written just below this offset
    _key:      PhantomData<K>,
    _val:      PhantomData<V>,
}

impl<'a, K: Key, V: Value> LeafPageBuilder<'a, K, V> {
    /// Zero the buffer, stamp page_type and page_id, initialise free pointers.
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

    /// Append a key-value pair. Keys must be pushed in strictly ascending order.
    ///
    /// Because entries arrive sorted we skip the binary-search lookup and
    /// simply append to the slot directory — O(1) vs the O(log n) insert path
    /// in LeafPageMutator. This matters during bulk loads and splits where
    /// many entries are written at once.
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

        debug_assert!(
            self.write_end >= free_start + SLOT_SIZE + rec_size,
            "LeafPageBuilder::push: page full"
        );

        // ── Write record ──────────────────────────────────────────────────
        self.write_end -= rec_size;
        let rec_base = self.write_end;

        write_u16(self.data, rec_base + REC_OFF_KEY_LEN, key_len as u16);
        write_u16(self.data, rec_base + REC_OFF_VAL_LEN, val_len as u16);
        write_u8 (self.data, rec_base + REC_OFF_FLAGS,   0);

        let key_off = rec_key_offset(rec_base);
        self.data[key_off..key_off + key_len].copy_from_slice(key_bytes);

        let val_off = rec_val_offset(rec_base, key_len);
        self.data[val_off..val_off + val_len].copy_from_slice(val_bytes);

        // ── Append slot ───────────────────────────────────────────────────
        let slot_off = slot_offset(n);
        write_u16(self.data, slot_off,     rec_base as u16);
        write_u16(self.data, slot_off + 2, rec_size as u16);

        // ── Update header ─────────────────────────────────────────────────
        write_u16(self.data, OFF_LEAF_SLOT_COUNT,  (n + 1) as u16);
        write_u16(self.data, OFF_LEAF_FREE_START,  (slot_off + SLOT_SIZE) as u16);
        write_u16(self.data, OFF_LEAF_FREE_END,    rec_base as u16);
    }

    /// Seal the page. Returns a LeafPageMutator for any remaining header
    /// writes before the page is handed to the buffer pool.
    pub fn finish(self) -> LeafPageMutator<'a, K, V> {
        LeafPageMutator::new(self.data)
    }
}
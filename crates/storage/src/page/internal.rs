//! Internal (branch) page types for the B+Tree storage engine.
//!
//! ## Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────┐
//! │ FIXED HEADER — 32 bytes (fully 8-byte aligned)          │
//! ├────────┬───────────┬──────────────────────────────────  ┤
//! │ Off  0 │ u8        │ page_type                           │
//! │ Off  1 │ u8        │ flags (dirty, root, etc.)           │
//! │ Off  2 │ u16       │ num_keys                            │
//! │ Off  4 │ u32       │ ← padding: brings header to 8 bytes │
//! ├────────┼───────────┼───────────────────────────────────  ┤
//! │ Off  8 │ u64       │ page_id          [8-byte aligned ✓] │
//! │ Off 16 │ u64       │ lsn              [8-byte aligned ✓] │
//! │ Off 24 │ u64       │ parent_page_id   [8-byte aligned ✓] │
//! └────────┴───────────┴───────────────────────────────────  ┘
//!
//! ┌──────────────────────────────────────────────────────────┐
//! │ SECTION A — Child page IDs  [(num_keys + 1) × 8 bytes]   │
//! │  child[i] offset = 32 + i*8                              │
//! └──────────────────────────────────────────────────────────┘
//!
//! ┌──────────────────────────────────────────────────────────┐
//! │ SECTION B — Key end offsets  [num_keys × 4 bytes]        │
//! │  key_end[i] = exclusive end of key[i] in Section C       │
//! └──────────────────────────────────────────────────────────┘
//!
//! ┌──────────────────────────────────────────────────────────┐
//! │ SECTION C — Key data  [variable, packed bytes]           │
//! │  key[i] spans [key_end[i-1], key_end[i])                 │
//! │  (key_end[-1] == 0 by convention)                        │
//! └──────────────────────────────────────────────────────────┘
//! ```

use std::cmp::Ordering;
use std::marker::PhantomData;
use common::Key;

use super::{
    read_u8, read_u16, read_u32, read_u64,
    write_u8, write_u16, write_u32, write_u64,
    ChildSide, Lsn, PageError, PageId,
    OFF_LSN, OFF_PAGE_ID, OFF_PAGE_TYPE, INTERNAL,
};

// ── Internal-page-specific header offsets ────────────────────────────────────

const OFF_INT_NUM_KEYS: usize = 2;   // u16
// bytes 4..8: u32 padding to keep the 8-byte fields aligned
const OFF_INT_PARENT:   usize = 24;  // u64
const INT_HEADER_SIZE:  usize = 32;

// ── Offset calculation helpers ────────────────────────────────────────────────

/// Byte offset of `child[i]` (Section A).
#[inline]
fn int_child_offset(i: usize) -> usize {
    INT_HEADER_SIZE + i * 8
}

/// Byte offset of the first byte of Section B (key-end offsets).
#[inline]
fn int_key_end_section(num_keys: usize) -> usize {
    INT_HEADER_SIZE + (num_keys + 1) * 8
}

/// Byte offset of `key_end[i]` (Section B entry).
#[inline]
fn int_key_end_offset(num_keys: usize, i: usize) -> usize {
    int_key_end_section(num_keys) + i * 4
}

/// Byte offset of the first byte of Section C (raw key data).
#[inline]
fn int_key_data_base(num_keys: usize) -> usize {
    int_key_end_section(num_keys) + num_keys * 4
}

// ── InternalPageAccessor ──────────────────────────────────────────────────────

/// Read-only typed view over a raw internal page buffer.
///
/// The lifetime `'a` is tied to the underlying page data, not to `&self`,
/// so `key_at` can return zero-copy `K::SelfType<'a>` values that outlive
/// the accessor itself.
pub struct InternalPageAccessor<'a, K: Key> {
    data: &'a [u8],
    _key: PhantomData<K>,
}

impl<'a, K: Key> InternalPageAccessor<'a, K> {
    /// Wraps a raw page buffer.
    ///
    /// # Panics
    /// Panics if the page-type byte does not equal [`INTERNAL`]. This indicates
    /// a programming error (wrong page handed to the wrong accessor).
    pub fn new(data: &'a [u8]) -> Self {
        assert_eq!(
            read_u8(data, OFF_PAGE_TYPE), INTERNAL,
            "InternalPageAccessor: page type byte is not INTERNAL"
        );
        Self { data, _key: PhantomData }
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

    /// Returns the parent page ID, or `None` if this is the root.
    pub fn parent_page_id(&self) -> Option<PageId> {
        match read_u64(self.data, OFF_INT_PARENT) {
            0 => None,
            v => Some(v),
        }
    }

    pub fn child_page_at(&self, i: usize) -> PageId {
        read_u64(self.data, int_child_offset(i))
    }

    /// Deserialised key at position `i`. Lifetime is `'a` (tied to page data).
    pub fn key_at(&self, i: usize) -> K::SelfType<'a> {
        K::from_bytes(self.key_bytes_at(i))
    }

    /// Binary-search for the child to descend into for `search_key`.
    ///
    /// Returns `(child_index, child_page_id)` where `child_index` is the first
    /// separator key that is strictly greater than `search_key`, which is the
    /// correct subtree to follow.
    pub fn find_child(&self, search_key: &K::SelfType<'_>) -> (usize, PageId) {
        let n = self.num_keys() as usize;
        let mut low = 0usize;
        let mut high = n;
        let search_bytes = K::as_bytes(search_key);
        let search_bytes = search_bytes.as_ref();

        while low < high {
            let mid = low + (high - low) / 2;
            match K::compare(self.key_bytes_at(mid), search_bytes) {
                Ordering::Greater              => high = mid,
                Ordering::Less | Ordering::Equal => low = mid + 1,
            }
        }
        // `low` is the index of the first separator > search_key,
        // which equals the child index to descend into.
        (low, self.child_page_at(low))
    }

    /// Contiguous free bytes remaining in this page.
    pub fn free_bytes(&self) -> usize {
        super::PAGE_SIZE - self.used_bytes()
    }

    /// Returns `true` if a new key of `key_len` bytes would fit on this page.
    pub fn can_fit(&self, key_len: usize) -> bool {
        // One more key+child needs:
        //   8 bytes  → child page ID (Section A)
        //   4 bytes  → key_end entry (Section B)
        //   key_len  → raw key bytes (Section C)
        self.free_bytes() >= 8 + 4 + key_len
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Raw bytes for `key[i]`. Lifetime `'a` lets `key_at` pass the slice
    /// directly to `K::from_bytes` without a copy.
    fn key_bytes_at(&self, i: usize) -> &'a [u8] {
        let n    = self.num_keys() as usize;
        let base = int_key_data_base(n);
        let start = if i == 0 {
            0
        } else {
            read_u32(self.data, int_key_end_offset(n, i - 1)) as usize
        };
        let end = read_u32(self.data, int_key_end_offset(n, i)) as usize;
        &self.data[base + start..base + end]
    }

    fn used_bytes(&self) -> usize {
        let n = self.num_keys() as usize;
        let key_data_size = if n == 0 {
            0
        } else {
            read_u32(self.data, int_key_end_offset(n, n - 1)) as usize
        };
        INT_HEADER_SIZE
            + (n + 1) * 8  // Section A: child pointers
            + n * 4         // Section B: key-end offsets
            + key_data_size // Section C: key data
    }
}

// ── InternalPageMutator ───────────────────────────────────────────────────────

/// Mutable typed view over a raw internal page buffer.
pub struct InternalPageMutator<'a, K: Key> {
    data: &'a mut [u8],
    _key: PhantomData<K>,
}

impl<'a, K: Key> InternalPageMutator<'a, K> {
    /// # Panics
    /// Panics if the page-type byte does not equal [`INTERNAL`].
    pub fn new(data: &'a mut [u8]) -> Self {
        assert_eq!(
            read_u8(data, OFF_PAGE_TYPE), INTERNAL,
            "InternalPageMutator: page type byte is not INTERNAL"
        );
        Self { data, _key: PhantomData }
    }

    pub fn set_lsn(&mut self, lsn: Lsn) {
        write_u64(self.data, OFF_LSN, lsn);
    }

    pub fn set_parent_page_id(&mut self, parent: Option<PageId>) {
        write_u64(self.data, OFF_INT_PARENT, parent.unwrap_or(0));
    }

    /// Update a child pointer in-place (e.g. after a split assigns a new ID).
    pub fn set_child_at(&mut self, i: usize, page_id: PageId) {
        write_u64(self.data, int_child_offset(i), page_id);
    }

    /// Borrow as a read-only accessor without releasing the mutable borrow.
    pub fn as_accessor(&self) -> InternalPageAccessor<'_, K> {
        InternalPageAccessor::new(self.data)
    }

    // ── Insert ────────────────────────────────────────────────────────────────

    /// Insert a promoted separator key at `index` after a child split.
    ///
    /// Before: `... | child[index] | key[index] | child[index+1] | ...`
    /// After:  `... | child[index] | key(new) | right_child | key[index] | ...`
    ///
    /// Returns `Err(InsufficientSpace)` when the page is too full.
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

    // ── Remove ────────────────────────────────────────────────────────────────

    /// Remove `key[index]` and one adjacent child during a merge.
    ///
    /// `keep` controls which of the two children flanking the key is retained.
    pub fn remove_key_at(&mut self, index: usize, keep: ChildSide) {
        let (mut children, mut keys) = self.snapshot();
        keys.remove(index);
        match keep {
            ChildSide::Left  => { children.remove(index + 1); }
            ChildSide::Right => { children.remove(index); }
        }
        self.rewrite(&children, &keys);
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Copy all children and keys into owned `Vec`s so they can be modified
    /// before being written back. Required because any change to `num_keys`
    /// shifts every section offset.
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

    /// Write children and keys back to the page using the correct section
    /// layout. `num_keys` is derived from `keys.len()`.
    fn rewrite(&mut self, children: &[PageId], keys: &[Vec<u8>]) {
        let new_n = keys.len();
        write_u16(self.data, OFF_INT_NUM_KEYS, new_n as u16);

        for (i, &child) in children.iter().enumerate() {
            write_u64(self.data, int_child_offset(i), child);
        }

        let mut cumulative = 0usize;
        for (i, key) in keys.iter().enumerate() {
            cumulative += key.len();
            write_u32(self.data, int_key_end_offset(new_n, i), cumulative as u32);
        }

        let base = int_key_data_base(new_n);
        let mut off = base;
        for key in keys {
            self.data[off..off + key.len()].copy_from_slice(key);
            off += key.len();
        }
    }
}

// ── InternalPageBuilder ───────────────────────────────────────────────────────
//
// Keys are buffered in `keys` and written all at once in `finish()` because
// every section offset depends on the final `num_keys`.

/// Write-once constructor for a fresh internal page.
pub struct InternalPageBuilder<'a, K: Key> {
    data:     &'a mut [u8],
    keys:     Vec<Vec<u8>>,
    children: Vec<PageId>,
    _key:     PhantomData<K>,
}

impl<'a, K: Key> InternalPageBuilder<'a, K> {
    /// Zero the buffer, stamp the page type and page ID.
    pub fn new(page_id: PageId, data: &'a mut [u8]) -> Self {
        data.fill(0);
        write_u8 (data, OFF_PAGE_TYPE, INTERNAL);
        write_u64(data, OFF_PAGE_ID,   page_id);
        Self { data, keys: Vec::new(), children: Vec::new(), _key: PhantomData }
    }

    /// Register the leftmost child. Must be called exactly once, before any
    /// `push_key_and_right_child` calls.
    ///
    /// # Panics
    /// Panics if called more than once (programming error).
    pub fn push_first_child(&mut self, child: PageId) {
        assert!(
            self.children.is_empty(),
            "InternalPageBuilder::push_first_child called more than once"
        );
        self.children.push(child);
    }

    /// Append a separator key and the right child that follows it.
    /// Keys must be pushed in strictly ascending order.
    ///
    /// # Panics
    /// Panics if `push_first_child` has not been called yet.
    pub fn push_key_and_right_child(&mut self, key: &K::SelfType<'_>, right_child: PageId) {
        assert!(
            !self.children.is_empty(),
            "InternalPageBuilder::push_key_and_right_child: call push_first_child first"
        );
        let key_bytes = K::as_bytes(key);
        self.keys.push(key_bytes.as_ref().to_vec());
        self.children.push(right_child);
    }

    /// Seal the page: flush all buffered data with the correct layout, then
    /// return a mutator for remaining header writes (lsn, parent).
    pub fn finish(self) -> InternalPageMutator<'a, K> {
        let Self { data, keys, children, .. } = self;
        let num_keys = keys.len();

        write_u16(data, OFF_INT_NUM_KEYS, num_keys as u16);

        for (i, &child) in children.iter().enumerate() {
            write_u64(data, int_child_offset(i), child);
        }

        let mut cumulative = 0usize;
        for (i, key) in keys.iter().enumerate() {
            cumulative += key.len();
            write_u32(data, int_key_end_offset(num_keys, i), cumulative as u32);
        }

        let base = int_key_data_base(num_keys);
        let mut off = base;
        for key in &keys {
            data[off..off + key.len()].copy_from_slice(key);
            off += key.len();
        }

        InternalPageMutator::new(data)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageBuffer;

    /// Build an internal page from a slice of (key_bytes, right_child) pairs
    /// with an explicit leftmost child.
    fn build_page(
        page_id:     u64,
        first_child: u64,
        entries:     &[(&[u8], u64)],
    ) -> PageBuffer {
        let mut buf = PageBuffer::new();
        let mut builder = InternalPageBuilder::<&[u8]>::new(page_id, buf.memory_mut());
        builder.push_first_child(first_child);
        for (key, child) in entries {
            builder.push_key_and_right_child(key, *child);
        }
        builder.finish();
        buf
    }

    #[test]
    fn build_and_read_keys_and_children() {
        let buf = build_page(
            1,
            10,
            &[(&[5], 20), (&[10], 30), (&[15], 40)],
        );
        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());

        assert_eq!(acc.page_id(), 1);
        assert_eq!(acc.num_keys(), 3);
        assert_eq!(acc.child_page_at(0), 10);
        assert_eq!(acc.child_page_at(1), 20);
        assert_eq!(acc.child_page_at(2), 30);
        assert_eq!(acc.child_page_at(3), 40);
        assert_eq!(acc.key_at(0), &[5u8][..]);
        assert_eq!(acc.key_at(1), &[10u8][..]);
        assert_eq!(acc.key_at(2), &[15u8][..]);
    }

    #[test]
    fn lsn_and_parent_round_trip() {
        let buf = build_page(7, 1, &[(&[42], 2)]);
        let mut buf = buf;
        let mut mutator = InternalPageMutator::<&[u8]>::new(buf.memory_mut());
        mutator.set_lsn(99);
        mutator.set_parent_page_id(Some(55));

        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.lsn(), 99);
        assert_eq!(acc.parent_page_id(), Some(55));
    }

    #[test]
    fn parent_page_id_none_when_zero() {
        let buf = build_page(1, 10, &[]);
        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.parent_page_id(), None);
    }

    #[test]
    fn find_child_binary_search() {
        // keys: [10, 20, 30]  children: [1, 2, 3, 4]
        let buf = build_page(
            1,
            1,
            &[(&[10], 2), (&[20], 3), (&[30], 4)],
        );
        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());

        // Anything < 10 → child 0
        let (idx, pid) = acc.find_child(&(&[5][..]));
        assert_eq!(idx, 0);
        assert_eq!(pid, 1);

        // Exactly 10 → child 1 (key[mid] == search: low = mid+1)
        let (idx, pid) = acc.find_child(&(&[10][..]));
        assert_eq!(idx, 1);
        assert_eq!(pid, 2);

        // Between 10 and 20 → child 1
        let (idx, pid) = acc.find_child(&(&[15][..]));
        assert_eq!(idx, 1);
        assert_eq!(pid, 2);

        // Exactly 30 → child 3
        let (idx, pid) = acc.find_child(&(&[30][..]));
        assert_eq!(idx, 3);
        assert_eq!(pid, 4);

        // Greater than all keys → child 3
        let (idx, pid) = acc.find_child(&(&[99][..]));
        assert_eq!(idx, 3);
        assert_eq!(pid, 4);
    }

    #[test]
    fn insert_key_and_right_child() {
        let mut buf = build_page(1, 10, &[(&[10], 20), (&[30], 40)]);
        {
            let mut m = InternalPageMutator::<&[u8]>::new(buf.memory_mut());
            m.insert_key_and_right_child(1, &(&[20][..]), 30).unwrap();
        }
        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.num_keys(), 3);
        assert_eq!(acc.key_at(0), &[10u8][..]);
        assert_eq!(acc.key_at(1), &[20u8][..]);
        assert_eq!(acc.key_at(2), &[30u8][..]);
        assert_eq!(acc.child_page_at(2), 30);
        assert_eq!(acc.child_page_at(3), 40);
    }

    #[test]
    fn insert_returns_err_when_full() {
        // Build a page, then fill it via repeated mutator inserts until a
        // further insert fails with InsufficientSpace.
        let mut buf = PageBuffer::new();
        {
            let mut b = InternalPageBuilder::<&[u8]>::new(1, buf.memory_mut());
            b.push_first_child(1);
            b.finish();
        }
        let big_key = vec![0u8; 200];
        let mut child_id = 2u64;
        loop {
            let mut m = InternalPageMutator::<&[u8]>::new(buf.memory_mut());
            if m.as_accessor().can_fit(big_key.len()) {
                let n = m.as_accessor().num_keys() as usize;
                m.insert_key_and_right_child(n, &big_key.as_slice(), child_id).unwrap();
                child_id += 1;
            } else {
                let result = m.insert_key_and_right_child(0, &big_key.as_slice(), 999);
                assert!(matches!(result, Err(PageError::InsufficientSpace { .. })));
                break;
            }
        }
    }

    #[test]
    fn remove_key_keep_left_child() {
        // keys: [10, 20]  children: [1, 2, 3]
        // Remove key[0]=10, keep child[0]=1 (drop child[1]=2)
        let mut buf = build_page(1, 1, &[(&[10], 2), (&[20], 3)]);
        InternalPageMutator::<&[u8]>::new(buf.memory_mut())
            .remove_key_at(0, ChildSide::Left);

        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.num_keys(), 1);
        assert_eq!(acc.key_at(0), &[20u8][..]);
        assert_eq!(acc.child_page_at(0), 1);
        assert_eq!(acc.child_page_at(1), 3);
    }

    #[test]
    fn remove_key_keep_right_child() {
        // keys: [10, 20]  children: [1, 2, 3]
        // Remove key[0]=10, keep child[1]=2 (drop child[0]=1)
        let mut buf = build_page(1, 1, &[(&[10], 2), (&[20], 3)]);
        InternalPageMutator::<&[u8]>::new(buf.memory_mut())
            .remove_key_at(0, ChildSide::Right);

        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.num_keys(), 1);
        assert_eq!(acc.key_at(0), &[20u8][..]);
        assert_eq!(acc.child_page_at(0), 2);
        assert_eq!(acc.child_page_at(1), 3);
    }

    #[test]
    fn set_child_at_updates_pointer() {
        let mut buf = build_page(1, 100, &[(&[5], 200)]);
        InternalPageMutator::<&[u8]>::new(buf.memory_mut()).set_child_at(1, 999);
        let acc = InternalPageAccessor::<&[u8]>::new(buf.memory());
        assert_eq!(acc.child_page_at(1), 999);
    }

    #[test]
    fn free_bytes_decreases_after_insert() {
        let mut buf = build_page(1, 1, &[]);
        let free_before = InternalPageAccessor::<&[u8]>::new(buf.memory()).free_bytes();
        InternalPageMutator::<&[u8]>::new(buf.memory_mut())
            .insert_key_and_right_child(0, &(&[42u8][..]), 2)
            .unwrap();
        let free_after = InternalPageAccessor::<&[u8]>::new(buf.memory()).free_bytes();
        // 8 (child ptr) + 4 (key_end entry) + 1 (key byte)
        assert_eq!(free_before - free_after, 8 + 4 + 1);
    }

    #[test]
    #[should_panic(expected = "InternalPageBuilder::push_first_child called more than once")]
    fn push_first_child_twice_panics() {
        let mut buf = PageBuffer::new();
        let mut b = InternalPageBuilder::<&[u8]>::new(1, buf.memory_mut());
        b.push_first_child(1);
        b.push_first_child(2); // must panic
    }

    #[test]
    #[should_panic(expected = "call push_first_child first")]
    fn push_key_without_first_child_panics() {
        let mut buf = PageBuffer::new();
        let mut b = InternalPageBuilder::<&[u8]>::new(1, buf.memory_mut());
        b.push_key_and_right_child(&(&[1][..]), 2); // must panic
    }
}

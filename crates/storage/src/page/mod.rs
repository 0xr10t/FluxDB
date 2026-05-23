//! B+Tree page layer.
//!
//! This module is split into two sub-modules:
//! - [`internal`] — branch (internal) page types
//! - [`leaf`]     — data (leaf) page types
//!
//! Shared constants, helpers, and error types live here.

pub mod internal;
pub mod leaf;

pub use internal::{InternalPageAccessor, InternalPageBuilder, InternalPageMutator};
pub use leaf::{LeafPageAccessor, LeafPageBuilder, LeafPageMutator};

pub use common::PageError;

// ── Type aliases ──────────────────────────────────────────────────────────────

pub type PageId = u64;
pub type Lsn = u64;
pub type SlotId = u16;

// ── Page-type marker bytes ────────────────────────────────────────────────────

/// Marker byte stored at offset 0 of every leaf page.
pub const LEAF: u8 = 1;
/// Marker byte stored at offset 0 of every internal page.
pub const INTERNAL: u8 = 2;

// ── Page size ─────────────────────────────────────────────────────────────────

/// Canonical page size used throughout the storage engine (4 KB).
pub const PAGE_SIZE: usize = 4096;

// ── Shared header offsets (present in both page types) ───────────────────────

pub(super) const OFF_PAGE_TYPE: usize = 0; // u8
// Byte 1 is reserved (was `flags`, never used).
pub(super) const OFF_PAGE_ID: usize = 8; // u64
pub(super) const OFF_LSN: usize = 16; // u64

// ── ChildSide — used when removing a separator key during a merge ─────────────

/// Which adjacent child to keep when a separator key is removed during a merge.
pub enum ChildSide {
    /// Keep `child[index]`, drop `child[index + 1]`.
    Left,
    /// Keep `child[index + 1]`, drop `child[index]`.
    Right,
}

// ── PageBuffer ────────────────────────────────────────────────────────────────

/// Heap-allocated, zero-initialised 4 KB page buffer.
///
/// Using a `Box<[u8; PAGE_SIZE]>` rather than a bare array prevents stack
/// overflows in debug builds while keeping the storage contiguous.
pub struct PageBuffer {
    data: Box<[u8; PAGE_SIZE]>,
}

impl PageBuffer {
    /// Allocates a new zeroed page buffer.
    pub fn new() -> Self {
        Self {
            // vec! avoids a stack-allocated [0; 4096] before moving to the Box.
            data: vec![0u8; PAGE_SIZE].into_boxed_slice().try_into().unwrap(),
        }
    }

    /// Read-only view of the raw bytes.
    pub fn memory(&self) -> &[u8] {
        self.data.as_ref()
    }

    /// Mutable view of the raw bytes.
    pub fn memory_mut(&mut self) -> &mut [u8] {
        self.data.as_mut()
    }
}

impl Default for PageBuffer {
    fn default() -> Self {
        Self::new()
    }
}

// ── Low-level read / write helpers ────────────────────────────────────────────
// These are plain functions (not methods) so both sub-modules can use them
// with a simple `use super::{read_u8, ...}` import.

#[inline]
pub(super) fn read_u8(data: &[u8], off: usize) -> u8 {
    data[off]
}

#[inline]
pub(super) fn read_u16(data: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(data[off..off + 2].try_into().unwrap())
}

#[inline]
pub(super) fn read_u32(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(data[off..off + 4].try_into().unwrap())
}

#[inline]
pub(super) fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap())
}

#[inline]
pub(super) fn write_u8(data: &mut [u8], off: usize, val: u8) {
    data[off] = val;
}

#[inline]
pub(super) fn write_u16(data: &mut [u8], off: usize, val: u16) {
    data[off..off + 2].copy_from_slice(&val.to_le_bytes());
}

#[inline]
pub(super) fn write_u32(data: &mut [u8], off: usize, val: u32) {
    data[off..off + 4].copy_from_slice(&val.to_le_bytes());
}

#[inline]
pub(super) fn write_u64(data: &mut [u8], off: usize, val: u64) {
    data[off..off + 8].copy_from_slice(&val.to_le_bytes());
}

//! B+Tree page layouts.
//!
//! This file defines *index* pages (B+Tree leaf/internal). These are not heap pages.
//! With inline rows, a leaf stores (key_bytes -> row_bytes) directly in the leaf page.
//!
//! On-disk approach: each page is a fixed-size byte buffer containing:
//! - header (fixed fields)
//! - slot directory (fixed-size slot entries; grows downward)
//! - payload area (variable-length key/value bytes; grows upward)

pub type PageId = u64;
pub type Lsn = u64; // Log Sequence Number (Pointer to corresponding log in WAL)

/// Page type tag stored in the page header.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BTreePageType {
    Internal = 1,
    Leaf = 2,
}

#[derive(Debug)]
pub enum PageError {}

pub type Result<T> = std::result::Result<T, PageError>;

const INVALID_PAGE_ID: u64 = u64::MAX;

// ---------------------------
// Common header (little-endian)
// ---------------------------
//
// Header layout (bytes):
// 0  : u8   page_type
// 1  : u8   reserved
// 2  : u16  slot_count
// 4  : u16  free_start
// 6  : u16  free_end
// 8  : u64  page_id
// 16 : u64  parent_page_id (or INVALID_PAGE_ID)
// 24 : u64  lsn
// 32 : u64  extra_0 (leaf: next_leaf, internal: first_child)
// 40 : u64  extra_1 (leaf: prev_leaf, internal: unused)
// 48 : ..   slot directory begins at HEADER_SIZE

pub struct LeafPage {
    buf: Vec<u8>,
}

impl LeafPage {
    pub new() -> Self {}

    // insert, update, get, delete functions
    // helpers to serialize and deserialize, get free space in page, etc
}

pub struct InternalPage {
    buf: Vec<u8>,
}

impl InternalPage {
    pub new() -> Self {}
}
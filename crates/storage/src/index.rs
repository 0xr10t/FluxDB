//! Functionality for managing B+Tree
//! Sort according to a key, add nodes, traverse the tree
//! to find result to a query, etc.
//!
//! Responsibilities (this module):
//! - Provide a stable API: get/insert/delete/range_scan.
//! - Navigate from root -> leaf using internal pages.
//! - Modify leaf/internal pages.
//! - Interact with BufferPool for page I/O and with WAL via BufferPool (WAL-before-page flush).

use std::sync::Arc;

use crate::{buffer_pool::BufferPool, page::PageId};

#[derive(Debug)]
pub enum IndexError {}

pub type Result<T> = std::result::Result<T, IndexError>;

/// Comparator for keys.
/// Since B Tree is sorted by comparison of keys
/// a method is needed to compare keys
pub trait KeyCmp: Send + Sync + 'static {
    fn cmp(&self, a: &[u8], b: &[u8]) -> std::cmp::Ordering;
}

/// Persistent metadata for an index.
#[derive(Debug, Clone, Copy)]
pub struct BTreeMeta {
    pub root: PageId,
}

/// B+Tree index (keys -> inline row bytes).
pub struct BTreeIndex {
    pool: Arc<BufferPool>,
    meta: BTreeMeta,
}

impl BTreeIndex {
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        todo!()
    }

    /// Insert (key -> row_bytes).
    pub fn insert(&self, key: &[u8], row: &[u8]) -> Result<()> {
        todo!()
    }

    /// Delete a key.
    pub fn delete(&self, key: &[u8]) -> Result<()> {
        todo!()
    }

    /// Traverse internal nodes to find the leaf that should contain `key`.
    fn find_leaf(&self, mut pid: PageId, key: &[u8]) -> Result<PageId> {
        todo!()
    }
}

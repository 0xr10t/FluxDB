//! Buffer Pool (page cache).
//!
//! - Cache fixed-size pages in memory (frames).
//! - Pin/unpin pages so in-use pages aren't evicted.
//! - Track dirty pages and flush them to disk.
//! - Enforce WAL-before-page flush: if a page is dirty with page_lsn = X,
//!   call wal.flush(X) before writing the page to disk.
 
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{
    disk::{DiskError, DiskManager},
    page::{Lsn, PageId},
    wal::{Wal, WalError},
};

#[derive(Debug)]
pub enum BufferPoolError {}

pub type Result<T> = std::result::Result<T, BufferPoolError>;


/// A pinned page handle. When dropped, it unpins the page.
pub struct PageHandle {
    inner: Arc<Mutex<Inner>>,
    frame_id: usize,
    page_id: PageId,
}

impl PageHandle {
    pub fn page_id(&self) -> PageId {}

    pub fn frame_id(&self) -> usize {}

    /// Read-only access to the page bytes.
    pub fn with_read<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {}

    /// Mutable access to the page bytes.
    pub fn with_write<R>(&self, f: impl FnOnce(&mut [u8]) -> R) -> R {}

    pub fn mark_dirty(&self) {}

    /// Set the page LSN after applying a change whose WAL record has LSN = `lsn`.
    pub fn set_page_lsn(&self, lsn: Lsn) {}

    pub fn page_lsn(&self) -> Lsn {}
}

impl Drop for PageHandle {}

/// BufferPool is an Arc+Mutex wrapper so handles can unpin on Drop.
pub struct BufferPool {
    inner: Arc<Mutex<Inner>>,
}

impl BufferPool {
    /// Fetch a page into the buffer pool and pin it
    pub fn fetch_page(&self, page_id: PageId) -> Result<PageHandle> {}

    /// Flush dirty page.
    pub fn flush_page(&self, page_id: PageId) -> Result<()> {}

    /// Flush all dirty pages.
    pub fn flush_all(&self) -> Result<()> {}
}

struct Inner {
    disk: DiskManager,
    wal: Wal,
    page_size: usize,
    page_table: HashMap<PageId, usize>,
    frames: Vec<Frame>,
    free_list: Vec<usize>,
    // // Simple CLOCK replacer state
    // clock_hand: usize,
}

impl Inner {}

struct Frame {
    page_id: Option<PageId>,
    data: Vec<u8>,
    pin_count: u32,
    dirty: bool,
    /// Page lsn that indicates the latest WAL record reflected on this page.
    page_lsn: Lsn,

    // /// CLOCK ref bit.
    // refbit: bool,
}

impl Frame {}
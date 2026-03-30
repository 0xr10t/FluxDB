//! # Buffer Pool Manager
//!
//! The Buffer Pool Manager is responsible for fetching database pages from disk
//! into memory and managing their lifecycles. It acts as an in-memory cache for
//! disk pages, ensuring that frequently accessed data remains in memory to
//! minimize expensive disk I/O.
//!
//! This implementation features:
//! - **Partitioning (Sharding)**: The pool is divided into multiple independent shards
//!   to reduce lock contention on the metadata table in high-concurrency environments.
//! - **RAII Page Guards**: Secure, automated reference counting (pinning/unpinning)
//!   using smart pointers (`PageReadGuard`, `PageWriteGuard`).
//! - **Deadlock Prevention**: Strict lock ordering for all operations.
//! - **Atomic Eviction**: Guaranteed data integrity during frame reuse with double-load detection.

use crate::disk::DiskManager;
use common::{INVALID_FRAME_ID, MAX_FRAMES, MAX_PAGE_SIZE, NUM_SHARDS, SHARD_MASK};
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard};

#[derive(Debug)]
pub enum BufferPoolError {
    PageNotFound(u64),
    PinCountError,
    NoEvictableFrames,
    InternalError(String),
}

impl Display for BufferPoolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferPoolError::PageNotFound(page_id) => {
                write!(f, "Page with ID {} not found", page_id)
            }
            BufferPoolError::PinCountError => write!(f, "Pin count error"),
            BufferPoolError::NoEvictableFrames => write!(f, "No evictable frames available"),
            BufferPoolError::InternalError(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for BufferPoolError {}

pub type Result<T> = std::result::Result<T, BufferPoolError>;

/// An implementation of the Clock replacement algorithm.
///
/// The `ClockReplacer` tracks which pages are currently in the buffer pool
/// and determines which page should be evicted when a new page needs to be loaded.
#[derive(Debug)]
struct ClockReplacer {
    size: usize,
    hand: usize,
    ref_bits: Vec<bool>,
    evictable: Vec<bool>,
}

impl ClockReplacer {
    /// Creates a new `ClockReplacer` with the specified capacity.
    fn new(size: usize) -> Self {
        Self {
            size,
            hand: 0,
            ref_bits: vec![false; size],
            evictable: vec![false; size],
        }
    }

    /// Finds a victim frame for eviction using the Clock algorithm.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::NoEvictableFrames`] if all frames are currently pinned.
    fn victim(&mut self) -> Result<usize> {
        let mut searched = 0;
        let size = self.size;
        while searched < 2 * size {
            let hand = self.hand;
            if self.evictable[hand] {
                if self.ref_bits[hand] {
                    self.ref_bits[hand] = false;
                } else {
                    self.evictable[hand] = false;
                    self.ref_bits[hand] = false;
                    self.hand = (hand + 1) % size;
                    return Ok(hand);
                }
            }
            self.hand = (hand + 1) % size;
            searched += 1;
        }
        Err(BufferPoolError::NoEvictableFrames)
    }

    /// Notifies the replacer that a frame has been unpinned and is now a candidate for eviction.
    fn unpin(&mut self, local_id: usize) {
        self.evictable[local_id] = true;
        self.ref_bits[local_id] = true;
    }

    /// Notifies the replacer that a frame has been pinned and cannot be evicted.
    fn pin(&mut self, local_id: usize) {
        self.evictable[local_id] = false;
        self.ref_bits[local_id] = false;
    }
}

/// A fixed-size buffer for a single database page.
///
/// This uses a heap-allocated `Box` to avoid stack overflow for large page sizes.
pub struct PageData(Box<[u8; MAX_PAGE_SIZE]>);

impl PageData {
    /// Creates a new, zeroed `PageData`.
    fn new() -> Self {
        Self(
            vec![0u8; MAX_PAGE_SIZE]
                .into_boxed_slice()
                .try_into()
                .unwrap(),
        )
    }
}

impl Deref for PageData {
    type Target = [u8; MAX_PAGE_SIZE];
    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl DerefMut for PageData {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut()
    }
}

/// An RAII guard for reading a page from the buffer pool.
///
/// When the guard is dropped, the page is automatically unpinned in the buffer pool.
pub struct PageReadGuard<'a> {
    shard: &'a BufferPoolShard,
    page_id: u64,
    guard: Option<RwLockReadGuard<'a, PageData>>,
}

impl<'a> Deref for PageReadGuard<'a> {
    type Target = [u8; MAX_PAGE_SIZE];
    fn deref(&self) -> &Self::Target {
        self.guard
            .as_ref()
            .expect("Guard should be present")
            .deref()
    }
}

impl<'a> Drop for PageReadGuard<'a> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        self.shard.unpin_page(self.page_id, false);
    }
}

/// An RAII guard for writing to a page in the buffer pool.
///
/// When the guard is dropped, the page is automatically unpinned and marked
/// as dirty if any modifications were made.
pub struct PageWriteGuard<'a> {
    shard: &'a BufferPoolShard,
    page_id: u64,
    guard: Option<RwLockWriteGuard<'a, PageData>>,
    dirty: bool,
}

impl<'a> Deref for PageWriteGuard<'a> {
    type Target = [u8; MAX_PAGE_SIZE];
    fn deref(&self) -> &Self::Target {
        self.guard
            .as_ref()
            .expect("Guard should be present")
            .deref()
    }
}

impl<'a> DerefMut for PageWriteGuard<'a> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.dirty = true;
        self.guard
            .as_mut()
            .expect("Guard should be present")
            .deref_mut()
    }
}

impl<'a> Drop for PageWriteGuard<'a> {
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
        }
        self.shard.unpin_page(self.page_id, self.dirty);
    }
}

/// Metadata for a single frame in a buffer pool shard.
struct FrameMetadata {
    /// The ID of the page currently residing in this frame.
    page_id: u64,
    /// Number of active pins for this frame.
    pin_count: u64,
    /// Whether the page in this frame has been modified.
    is_dirty: bool,
}

/// A shard of the buffer pool, managing a subset of the total frames.
///
/// Sharding reduces lock contention by allowing concurrent access to different
/// parts of the buffer pool.
struct BufferPoolShard {
    disk_manager: Arc<DiskManager>,
    pages: Vec<RwLock<PageData>>,
    inner: Mutex<ShardInner>,
}

struct ShardInner {
    metadata: Vec<FrameMetadata>,
    page_table: HashMap<u64, usize>,
    free_list: Vec<usize>,
    replacer: ClockReplacer,
}

impl BufferPoolShard {
    /// Creates a new `BufferPoolShard` with the specified number of frames.
    fn new(disk_manager: Arc<DiskManager>, size: usize) -> Self {
        let mut metadata = Vec::with_capacity(size);
        let mut free_list = Vec::with_capacity(size);
        for frame_id in 0..size {
            metadata.push(FrameMetadata {
                page_id: INVALID_FRAME_ID,
                pin_count: 0,
                is_dirty: false,
            });
            free_list.push(size - 1 - frame_id);
        }

        Self {
            disk_manager,
            pages: (0..size).map(|_| RwLock::new(PageData::new())).collect(),
            inner: Mutex::new(ShardInner {
                metadata,
                page_table: HashMap::with_capacity(size),
                free_list,
                replacer: ClockReplacer::new(size),
            }),
        }
    }

    /// Decrements the pin count of a page.
    ///
    /// If the pin count reaches zero, the frame becomes a candidate for eviction.
    fn unpin_page(&self, page_id: u64, is_dirty: bool) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&local_id) = inner.page_table.get(&page_id) {
            let meta = &mut inner.metadata[local_id];
            if meta.pin_count > 0 {
                meta.pin_count -= 1;
                meta.is_dirty |= is_dirty;
                if meta.pin_count == 0 {
                    inner.replacer.unpin(local_id);
                }
            }
        }
    }

    /// Increments the pin count of a page if it is already in the shard.
    ///
    /// Returns the frame ID if the page was found and pinned.
    fn pin_page(&self, page_id: u64) -> Option<usize> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&frame_id) = inner.page_table.get(&page_id) {
            inner.metadata[frame_id].pin_count += 1;
            inner.replacer.pin(frame_id);
            Some(frame_id)
        } else {
            None
        }
    }

    /// Finds a frame to be used for a new page, either from the free list or by eviction.
    fn find_victim_frame_id(&self, inner: &mut ShardInner) -> Result<usize> {
        if let Some(id) = inner.free_list.pop() {
            Ok(id)
        } else {
            inner.replacer.victim()
        }
    }

    /// Evicts a page if necessary and replaces it with the requested page.
    ///
    /// Returns the frame ID and a boolean indicating if the page needs to be loaded from disk.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::NoEvictableFrames`] if no frames can be evicted.
    fn evict_and_replace(&self, page_id: u64) -> Result<(usize, bool)> {
        loop {
            let mut inner = self.inner.lock().unwrap();

            // Re-check page_table after acquiring lock to prevent double-loading
            if let Some(&local_id) = inner.page_table.get(&page_id) {
                inner.metadata[local_id].pin_count += 1;
                inner.replacer.pin(local_id);
                return Ok((local_id, false));
            }

            let frame_id = self.find_victim_frame_id(&mut inner)?;
            let meta = &inner.metadata[frame_id];
            let old_page_id = meta.page_id;
            let is_dirty = meta.is_dirty;

            if is_dirty && old_page_id != INVALID_FRAME_ID {
                drop(inner);
                self.flush_page(old_page_id)?;
                continue;
            }

            if old_page_id != INVALID_FRAME_ID {
                inner.page_table.remove(&old_page_id);
            }

            let meta = &mut inner.metadata[frame_id];
            meta.page_id = page_id;
            meta.pin_count = 1;
            meta.is_dirty = false;

            inner.page_table.insert(page_id, frame_id);
            inner.replacer.pin(frame_id);

            return Ok((frame_id, true));
        }
    }

    /// Flushes a specific page to disk if it is dirty.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::InternalError`] if an I/O error occurs.
    fn flush_page(&self, page_id: u64) -> Result<()> {
        let (pid, frame_id) = {
            let mut inner = self.inner.lock().unwrap();
            if let Some(&id) = inner.page_table.get(&page_id) {
                let meta = &mut inner.metadata[id];
                if meta.is_dirty && meta.page_id != INVALID_FRAME_ID {
                    meta.is_dirty = false;
                    meta.pin_count += 1;
                    (meta.page_id, id)
                } else {
                    return Ok(());
                }
            } else {
                return Ok(());
            }
        };

        let res = self.write_frame_to_disk(frame_id, pid);

        let mut inner = self.inner.lock().unwrap();
        let meta = &mut inner.metadata[frame_id];
        meta.pin_count -= 1;
        if meta.pin_count == 0 {
            inner.replacer.unpin(frame_id);
        }
        if let Err(e) = res {
            inner.metadata[frame_id].is_dirty = true;
            return Err(e);
        }

        Ok(())
    }

    /// Flushes all dirty pages in this shard to disk.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::InternalError`] if an I/O error occurs.
    fn flush_all_pages(&self) -> Result<()> {
        let n_frames = self.pages.len();
        for frame_id in 0..n_frames {
            let (pid, is_dirty) = {
                let inner = self.inner.lock().unwrap();
                let meta = &inner.metadata[frame_id];
                (meta.page_id, meta.is_dirty)
            };
            if is_dirty && pid != INVALID_FRAME_ID {
                self.flush_page(pid)?;
            }
        }
        Ok(())
    }

    /// Writes the contents of a frame to its corresponding location on disk.
    fn write_frame_to_disk(&self, frame_id: usize, page_id: u64) -> Result<()> {
        let mut buf = vec![0u8; MAX_PAGE_SIZE];
        {
            let data = self.pages[frame_id].read().unwrap();
            buf.copy_from_slice(&data[..]);
        }

        self.disk_manager
            .write_page(page_id, &buf)
            .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
        self.disk_manager
            .sync_data()
            .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
        Ok(())
    }

    /// Deletes a page from the shard, freeing its frame.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::PinCountError`] if the page is currently pinned.
    fn delete_page(&self, page_id: u64) -> Result<()> {
        loop {
            let mut inner = self.inner.lock().unwrap();
            let frame_id = match inner.page_table.get(&page_id) {
                Some(&id) => id,
                None => return Ok(()),
            };

            if inner.metadata[frame_id].pin_count > 0 {
                return Err(BufferPoolError::PinCountError);
            }

            if inner.metadata[frame_id].is_dirty {
                inner.metadata[frame_id].pin_count += 1;
                let pid = inner.metadata[frame_id].page_id;
                drop(inner);

                let res = self.write_frame_to_disk(frame_id, pid);

                let mut inner = self.inner.lock().unwrap();
                let meta = &mut inner.metadata[frame_id];
                meta.pin_count -= 1;
                if meta.pin_count == 0 {
                    inner.replacer.unpin(frame_id);
                }
                if let Err(_) = res {
                    inner.metadata[frame_id].is_dirty = true;
                    return Err(BufferPoolError::InternalError(
                        "Flush failed during delete".to_string(),
                    ));
                }
                inner.metadata[frame_id].is_dirty = false;
                drop(inner);
                continue;
            }

            inner.page_table.remove(&page_id);
            let meta = &mut inner.metadata[frame_id];
            meta.page_id = INVALID_FRAME_ID;
            meta.pin_count = 0;
            meta.is_dirty = false;
            inner.replacer.pin(frame_id);
            inner.free_list.push(frame_id);

            return Ok(());
        }
    }
}

/// The main manager for the buffer pool, providing a partitioned cache for disk pages.
///
/// It coordinates multiple `BufferPoolShard` instances to minimize lock contention
/// and provides a high-level interface for fetching and creating pages.
pub struct BufferPoolManager {
    shards: Vec<BufferPoolShard>,
    next_page_id: Mutex<u64>,
}

impl BufferPoolManager {
    /// Creates a new `BufferPoolManager` using the provided `DiskManager`.
    pub fn new(disk_manager: Arc<DiskManager>) -> Self {
        let existing_pages = disk_manager.num_pages().unwrap_or(0);

        let shard_size = MAX_FRAMES / NUM_SHARDS;
        let shards = (0..NUM_SHARDS)
            .map(|_| BufferPoolShard::new(disk_manager.clone(), shard_size))
            .collect();

        Self {
            shards,
            next_page_id: Mutex::new(existing_pages),
        }
    }

    /// Returns the shard index for the given page ID.
    #[inline]
    fn get_shard(&self, page_id: u64) -> &BufferPoolShard {
        &self.shards[(page_id & SHARD_MASK) as usize]
    }

    /// Checks if a page ID is valid (i.e., it has been allocated).
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::PageNotFound`] if the page ID is out of bounds.
    fn check_page_id(&self, page_id: u64) -> Result<()> {
        let next_id = *self.next_page_id.lock().unwrap();
        if page_id >= next_id {
            Err(BufferPoolError::PageNotFound(page_id))
        } else {
            Ok(())
        }
    }

    /// Creates a new page in the buffer pool.
    ///
    /// The new page is automatically pinned and zero-initialized.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::NoEvictableFrames`] if no frames are available for eviction.
    pub fn new_page(&self) -> Result<PageWriteGuard<'_>> {
        let page_id = {
            let mut id = self.next_page_id.lock().unwrap();
            let pid = *id;
            *id += 1;
            pid
        };

        let shard = self.get_shard(page_id);
        let (frame_id, _) = shard.evict_and_replace(page_id)?;

        let mut data = shard.pages[frame_id].write().unwrap();
        data.fill(0);

        Ok(PageWriteGuard {
            shard,
            page_id,
            guard: Some(data),
            dirty: false,
        })
    }

    /// Fetches a page from the buffer pool for reading.
    ///
    /// If the page is not in the pool, it is loaded from disk. The page is
    /// automatically pinned upon return.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::PageNotFound`] if the page ID is invalid,
    /// or [`BufferPoolError::NoEvictableFrames`] if no frames are available.
    pub fn fetch_page(&self, page_id: u64) -> Result<PageReadGuard<'_>> {
        self.check_page_id(page_id)?;
        let shard = self.get_shard(page_id);

        if let Some(frame_id) = shard.pin_page(page_id) {
            let data = shard.pages[frame_id].read().unwrap();
            return Ok(PageReadGuard {
                shard,
                page_id,
                guard: Some(data),
            });
        }

        let (frame_id, needs_load) = shard.evict_and_replace(page_id)?;

        if needs_load {
            let mut data = shard.pages[frame_id].write().unwrap();
            shard
                .disk_manager
                .read_page(page_id, data.0.as_mut())
                .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
        }

        let data = shard.pages[frame_id].read().unwrap();
        Ok(PageReadGuard {
            shard,
            page_id,
            guard: Some(data),
        })
    }

    /// Fetches a page from the buffer pool for writing.
    ///
    /// Similar to `fetch_page`, but returns a write guard that marks the
    /// page as dirty when dropped if modified.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::PageNotFound`] if the page ID is invalid,
    /// or [`BufferPoolError::NoEvictableFrames`] if no frames are available.
    pub fn fetch_page_mut(&self, page_id: u64) -> Result<PageWriteGuard<'_>> {
        self.check_page_id(page_id)?;
        let shard = self.get_shard(page_id);

        if let Some(frame_id) = shard.pin_page(page_id) {
            let data = shard.pages[frame_id].write().unwrap();
            return Ok(PageWriteGuard {
                shard,
                page_id,
                guard: Some(data),
                dirty: false,
            });
        }

        let (frame_id, needs_load) = shard.evict_and_replace(page_id)?;

        let mut data = shard.pages[frame_id].write().unwrap();
        if needs_load {
            shard
                .disk_manager
                .read_page(page_id, data.0.as_mut())
                .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
        }

        Ok(PageWriteGuard {
            shard,
            page_id,
            guard: Some(data),
            dirty: false,
        })
    }

    /// Flushes a specific page to disk if it is dirty.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::InternalError`] if an I/O error occurs.
    pub fn flush_page(&self, page_id: u64) -> Result<()> {
        self.get_shard(page_id).flush_page(page_id)
    }

    /// Flushes all dirty pages in the buffer pool to disk.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::InternalError`] if an I/O error occurs in any shard.
    pub fn flush_all_pages(&self) -> Result<()> {
        for shard in &self.shards {
            shard.flush_all_pages()?;
        }
        Ok(())
    }

    /// Deletes a page from the buffer pool and disk management.
    ///
    /// # Errors
    ///
    /// Returns [`BufferPoolError::PinCountError`] if the page is currently pinned.
    pub fn delete_page(&self, page_id: u64) -> Result<()> {
        self.get_shard(page_id).delete_page(page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_clock_replacer_eviction_order() {
        let mut replacer = ClockReplacer::new(4);
        replacer.unpin(0);
        replacer.unpin(1);
        replacer.unpin(2);
        replacer.unpin(3);
        let v1 = replacer.victim().unwrap();
        assert_eq!(v1, 0);
        let v2 = replacer.victim().unwrap();
        assert_eq!(v2, 1);
        replacer.pin(2);
        let v3 = replacer.victim().unwrap();
        assert_eq!(v3, 3);
        let result = replacer.victim();
        assert!(result.is_err());
    }

    #[test]
    fn test_buffer_pool_manager_basic() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk_manager = Arc::new(DiskManager::new(&path, MAX_PAGE_SIZE).unwrap());
        let bpm = BufferPoolManager::new(disk_manager);
        let page_id;
        {
            let mut page = bpm.new_page().unwrap();
            page_id = page.page_id;
            page[0] = 1;
            page[1] = 2;
        }
        {
            let page = bpm.fetch_page(page_id).unwrap();
            assert_eq!(page[0], 1);
            assert_eq!(page[1], 2);
        }
        {
            let mut page = bpm.fetch_page_mut(page_id).unwrap();
            page[0] = 3;
        }
        {
            let page = bpm.fetch_page(page_id).unwrap();
            assert_eq!(page[0], 3);
        }
    }

    #[test]
    fn test_buffer_pool_manager_eviction_persistence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk_manager = Arc::new(DiskManager::new(&path, MAX_PAGE_SIZE).unwrap());
        let bpm = BufferPoolManager::new(disk_manager);
        let mut page_ids = Vec::new();
        for i in 0..MAX_FRAMES + 1 {
            let mut page = bpm.new_page().unwrap();
            page[0] = i as u8;
            page_ids.push(page.page_id);
        }
        for i in 0..MAX_FRAMES + 1 {
            let page = bpm.fetch_page(page_ids[i]).unwrap();
            assert_eq!(page[0], i as u8);
        }
    }

    #[test]
    fn test_buffer_pool_manager_full_lifecycle() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk_manager = Arc::new(DiskManager::new(&path, MAX_PAGE_SIZE).unwrap());
        let bpm = BufferPoolManager::new(disk_manager);
        let mut page_ids = Vec::new();
        for i in 0..10 {
            let mut page = bpm.new_page().unwrap();
            let pid = page.page_id;
            page[0] = i as u8;
            page_ids.push(pid);
        }
        for i in 0..10 {
            let page = bpm.fetch_page(page_ids[i]).unwrap();
            assert_eq!(page[0], i as u8);
        }
        for i in 0..10 {
            let mut page = bpm.fetch_page_mut(page_ids[i]).unwrap();
            page[0] = (i + 10) as u8;
        }
        for i in 0..10 {
            bpm.flush_page(page_ids[i]).unwrap();
        }
        for i in 0..10 {
            let page = bpm.fetch_page(page_ids[i]).unwrap();
            assert_eq!(page[0], (i + 10) as u8);
        }
    }

    #[test]
    fn test_buffer_pool_manager_concurrency() {
        use std::thread;
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk_manager = Arc::new(DiskManager::new(&path, MAX_PAGE_SIZE).unwrap());
        let bpm = Arc::new(BufferPoolManager::new(disk_manager));
        let mut handles = Vec::new();
        for i in 0..10 {
            let bpm_clone = bpm.clone();
            handles.push(thread::spawn(move || {
                let mut page = bpm_clone.new_page().unwrap();
                page[0] = i as u8;
                let pid = page.page_id;
                drop(page);
                let fetched = bpm_clone.fetch_page(pid).unwrap();
                assert_eq!(fetched[0], i as u8);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
    }

    #[test]
    fn test_buffer_pool_manager_pin_count() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.db");
        let disk_manager = Arc::new(DiskManager::new(&path, MAX_PAGE_SIZE).unwrap());
        let bpm = BufferPoolManager::new(disk_manager);
        let mut pages = Vec::new();
        for _ in 0..MAX_FRAMES {
            pages.push(bpm.new_page().unwrap());
        }
        let res = bpm.new_page();
        assert!(matches!(res, Err(BufferPoolError::NoEvictableFrames)));
        drop(pages);
        assert!(bpm.new_page().is_ok());
    }
}

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

#[derive(Debug)]
struct ClockReplacer {
    size: usize,
    hand: usize,
    ref_bits: Vec<bool>,
    evictable: Vec<bool>,
}

impl ClockReplacer {
    fn new(size: usize) -> Self {
        Self {
            size,
            hand: 0,
            ref_bits: vec![false; size],
            evictable: vec![false; size],
        }
    }

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

    fn unpin(&mut self, local_id: usize) {
        self.evictable[local_id] = true;
        self.ref_bits[local_id] = true;
    }

    fn pin(&mut self, local_id: usize) {
        self.evictable[local_id] = false;
        self.ref_bits[local_id] = false;
    }
}

// Used heap allocation for page data to avoid stack overflow
pub struct PageData(Box<[u8; MAX_PAGE_SIZE]>);

impl PageData {
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

struct FrameMetadata {
    page_id: u64,
    pin_count: u64,
    is_dirty: bool,
}

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

    fn find_victim_frame_id(&self, inner: &mut ShardInner) -> Result<usize> {
        if let Some(id) = inner.free_list.pop() {
            Ok(id)
        } else {
            inner.replacer.victim()
        }
    }

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
        inner.metadata[frame_id].pin_count -= 1;
        if let Err(e) = res {
            inner.metadata[frame_id].is_dirty = true;
            return Err(e);
        }

        Ok(())
    }

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
                inner.metadata[frame_id].pin_count -= 1;
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

pub struct BufferPoolManager {
    shards: Vec<BufferPoolShard>,
    next_page_id: Mutex<u64>,
}

impl BufferPoolManager {
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

    #[inline]
    fn get_shard(&self, page_id: u64) -> &BufferPoolShard {
        &self.shards[(page_id & SHARD_MASK) as usize]
    }

    fn check_page_id(&self, page_id: u64) -> Result<()> {
        let next_id = *self.next_page_id.lock().unwrap();
        if page_id >= next_id {
            Err(BufferPoolError::PageNotFound(page_id))
        } else {
            Ok(())
        }
    }

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

    pub fn flush_page(&self, page_id: u64) -> Result<()> {
        self.get_shard(page_id).flush_page(page_id)
    }

    pub fn flush_all_pages(&self) -> Result<()> {
        for shard in &self.shards {
            shard.flush_all_pages()?;
        }
        Ok(())
    }

    pub fn delete_page(&self, page_id: u64) -> Result<()> {
        self.get_shard(page_id).delete_page(page_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_replacer_eviction_order() {
        let mut replacer = ClockReplacer::new(4);

        replacer.unpin(0);
        replacer.unpin(1);
        replacer.unpin(2);
        replacer.unpin(3);

        let v1 = replacer.victim().unwrap();
        assert_eq!(v1, 0, "First eviction should be frame 0");

        let v2 = replacer.victim().unwrap();
        assert_eq!(v2, 1, "Second eviction should be frame 1");

        replacer.pin(2);
        let v3 = replacer.victim().unwrap();
        assert_eq!(v3, 3, "Should skip pinned frame 2 and evict frame 3");
        let result = replacer.victim();
        assert!(
            result.is_err(),
            "Should return NoEvictableFrames when only pinned frames remain"
        );
    }
}

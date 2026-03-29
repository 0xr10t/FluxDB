use crate::disk::DiskManager;
use common::{INVALID_FRAME_ID, MAX_FRAMES, MAX_PAGE_SIZE};
use core::array::from_fn;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, Mutex, RwLock};

#[derive(Debug, PartialEq)]
pub enum BufferPoolError {
    PageNotFound(u64),
    PinCountError,
    NotEvictable(u64),
    InternalError(String),
}

impl Display for BufferPoolError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferPoolError::PageNotFound(page_id) => {
                write!(f, "Page with page id: {} not found", page_id)
            }
            BufferPoolError::PinCountError => write!(f, "Pin count cannot be negative"),
            BufferPoolError::NotEvictable(frame_id) => {
                write!(f, "Frame id: {} is not evictable", frame_id)
            }
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
    pub fn new(size: usize) -> Self {
        Self {
            size,
            hand: 0,
            ref_bits: vec![false; size],
            evictable: vec![false; size],
        }
    }

    pub fn victim(&mut self) -> Result<u64> {
        let mut searched = 0;
        while searched < 2 * self.size {
            if self.evictable[self.hand] {
                if self.ref_bits[self.hand] {
                    self.ref_bits[self.hand] = false;
                } else {
                    let victim_id = self.hand as u64;
                    self.hand = (self.hand + 1) % self.size;
                    return Ok(victim_id);
                }
            }
            self.hand = (self.hand + 1) % self.size;
            searched += 1;
        }
        Err(BufferPoolError::NotEvictable(0))
    }

    pub fn unpin(&mut self, frame_id: u64) {
        let idx = frame_id as usize;
        self.evictable[idx] = true;
        self.ref_bits[idx] = true;
    }

    pub fn pin(&mut self, frame_id: u64) {
        let idx = frame_id as usize;
        self.evictable[idx] = false;
        self.ref_bits[idx] = false;
    }
}

#[derive(Clone, Debug)]
pub struct Page {
    pub id: u64,
    pub pin_count: u64,
    pub is_dirty: bool,
    pub data: [u8; MAX_PAGE_SIZE],
}

impl Page {
    fn new() -> Self {
        Self {
            id: INVALID_FRAME_ID,
            pin_count: 0,
            is_dirty: false,
            data: [0u8; MAX_PAGE_SIZE],
        }
    }
}

pub struct BufferPoolManager {
    disk_manager: Arc<DiskManager>,
    pages: [RwLock<Page>; MAX_FRAMES],
    inner: Mutex<Inner>,
}

struct Inner {
    page_table: HashMap<u64, u64>,
    free_list: Vec<u64>,
    replacer: ClockReplacer,
    next_page_id: u64,
}

impl BufferPoolManager {
    pub fn new(disk_manager: DiskManager) -> Self {
        let mut free_list = Vec::with_capacity(MAX_FRAMES);
        for i in (0..MAX_FRAMES).rev() {
            free_list.push(i as u64);
        }

        Self {
            disk_manager: Arc::new(disk_manager),
            pages: from_fn(|_| RwLock::new(Page::new())),
            inner: Mutex::new(Inner {
                replacer: ClockReplacer::new(MAX_FRAMES),
                free_list,
                page_table: HashMap::with_capacity(MAX_FRAMES),
                next_page_id: 0,
            }),
        }
    }

    fn find_frame(&self) -> Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        
        if let Some(idx) = inner.free_list.pop() {
            inner.replacer.pin(idx);
            Ok(idx)
        } else {
            let victim_idx = inner.replacer.victim()?;
            let victim_id = self.pages[victim_idx as usize].read().unwrap().id;
            
            inner.page_table.remove(&victim_id);
            inner.replacer.pin(victim_idx);
            Ok(victim_idx)
        }
    }

    pub fn new_page(&self) -> Result<u64> {
        let frame_id = self.find_frame()?;
        
        {
            let mut page = self.pages[frame_id as usize].write().unwrap();
            if page.is_dirty {
                self.disk_manager
                    .write_page(page.id, &page.data)
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
                self.disk_manager
                    .sync_data()
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
                page.is_dirty = false;
            }

            let page_id = {
                let mut inner = self.inner.lock().unwrap();
                let pid = inner.next_page_id;
                inner.next_page_id += 1;
                inner.page_table.insert(pid, frame_id);
                pid
            };

            page.id = page_id;
            page.pin_count = 1;
            page.is_dirty = false;
            page.data.fill(0);
        }

        Ok(frame_id)
    }

    pub fn fetch_page(&self, page_id: u64) -> Result<u64> {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(&frame_id) = inner.page_table.get(&page_id) {
                let mut page = self.pages[frame_id as usize].write().unwrap();
                page.pin_count += 1;
                inner.replacer.pin(frame_id);
                return Ok(frame_id);
            }
        }

        let frame_id = self.find_frame()?;
        
        {
            let mut page = self.pages[frame_id as usize].write().unwrap();
            
            if page.is_dirty {
                self.disk_manager
                    .write_page(page.id, &page.data)
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
                self.disk_manager
                    .sync_data()
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
                page.is_dirty = false;
            }

            self.disk_manager
                .read_page(page_id, &mut page.data)
                .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;

            page.id = page_id;
            page.pin_count = 1;
            page.is_dirty = false;

            let mut inner = self.inner.lock().unwrap();
            inner.page_table.insert(page_id, frame_id);
        }

        Ok(frame_id)
    }

    pub fn flush_page(&self, page_id: u64) -> Result<bool> {
        let frame_id = {
            let inner = self.inner.lock().unwrap();
            inner.page_table.get(&page_id).copied()
        };

        if let Some(frame_id) = frame_id {
            let mut page = self.pages[frame_id as usize].write().unwrap();
            if page.id != page_id {
                return Ok(false);
            }
            
            self.disk_manager
                .write_page(page.id, &page.data)
                .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
            self.disk_manager
                .sync_data()
                .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
            page.is_dirty = false;
            Ok(true)
        } else {
            Err(BufferPoolError::PageNotFound(page_id))
        }
    }

    pub fn flush_all_pages(&self) -> Result<()> {
        let page_ids: Vec<u64> = self.inner.lock().unwrap().page_table.keys().cloned().collect();
        for pid in page_ids {
            self.flush_page(pid)?;
        }
        Ok(())
    }

    pub fn unpin_page(&self, page_id: u64, is_dirty: bool) -> Result<()> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(&frame_id) = inner.page_table.get(&page_id) {
            let mut page = self.pages[frame_id as usize].write().unwrap();
            if page.pin_count == 0 {
                return Err(BufferPoolError::PinCountError);
            }
            page.pin_count -= 1;
            page.is_dirty |= is_dirty;
            if page.pin_count == 0 {
                inner.replacer.unpin(frame_id);
            }
            Ok(())
        } else {
            Err(BufferPoolError::PageNotFound(page_id))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_replacer() {
        let mut replacer = ClockReplacer::new(3);
        replacer.unpin(0);
        replacer.unpin(1);
        replacer.unpin(2);

        // Use victim to rotate the hand
        assert_eq!(replacer.victim().unwrap(), 0);
        assert_eq!(replacer.victim().unwrap(), 1);

        replacer.pin(2);
        assert_eq!(replacer.victim().unwrap(), 0);
    }
}

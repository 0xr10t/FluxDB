use crate::disk::DiskManager;
use common::{MAX_FRAMES, MAX_PAGE_SIZE};
use core::array::from_fn;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};

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
            id: 0,
            pin_count: 0,
            is_dirty: false,
            data: [0u8; MAX_PAGE_SIZE],
        }
    }
}

pub struct BufferPoolManager {
    disk_manager: DiskManager,
    pages: [Page; MAX_FRAMES],
    replacer: ClockReplacer,
    free_list: Vec<u64>,
    page_table: HashMap<u64, u64>,
    next_page_id: u64,
}

impl BufferPoolManager {
    pub fn new(disk_manager: DiskManager) -> Self {
        let mut free_list = Vec::with_capacity(MAX_FRAMES);
        for i in (0..MAX_FRAMES).rev() {
            free_list.push(i as u64);
        }

        Self {
            disk_manager,
            pages: from_fn(|_| Page::new()),
            replacer: ClockReplacer::new(MAX_FRAMES),
            free_list,
            page_table: HashMap::with_capacity(MAX_FRAMES),
            next_page_id: 0,
        }
    }

    pub fn page(&self, frame_id: u64) -> &Page {
        &self.pages[frame_id as usize]
    }

    pub fn page_mut(&mut self, frame_id: u64) -> &mut Page {
        &mut self.pages[frame_id as usize]
    }

    fn find_frame(&mut self) -> Result<u64> {
        if let Some(idx) = self.free_list.pop() {
            Ok(idx)
        } else {
            let victim_idx = self.replacer.victim()?;
            let victim_page = &mut self.pages[victim_idx as usize];

            if victim_page.is_dirty {
                self.disk_manager
                    .write_page(victim_page.id, &victim_page.data)
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
                self.disk_manager
                    .sync_data()
                    .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;
            }

            self.page_table.remove(&victim_page.id);
            Ok(victim_idx)
        }
    }

    pub fn new_page(&mut self) -> Result<u64> {
        let frame_id = self.find_frame()?;
        let page_id = self.next_page_id;
        self.next_page_id += 1;

        let page = &mut self.pages[frame_id as usize];
        page.id = page_id;
        page.pin_count = 1;
        page.is_dirty = false;
        page.data.fill(0);

        self.page_table.insert(page_id, frame_id);
        self.replacer.pin(frame_id);

        Ok(frame_id)
    }

    pub fn fetch_page(&mut self, page_id: u64) -> Result<u64> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id as usize];
            page.pin_count += 1;
            self.replacer.pin(frame_id);
            return Ok(frame_id);
        }

        let frame_id = self.find_frame()?;
        let page = &mut self.pages[frame_id as usize];
        page.id = page_id;
        page.pin_count = 1;
        page.is_dirty = false;

        self.disk_manager
            .read_page(page_id, &mut page.data)
            .map_err(|e| BufferPoolError::InternalError(e.to_string()))?;

        self.page_table.insert(page_id, frame_id);
        self.replacer.pin(frame_id);

        Ok(frame_id)
    }

    pub fn flush_page(&mut self, page_id: u64) -> Result<bool> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id as usize];
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

    pub fn flush_all_pages(&mut self) -> Result<()> {
        let page_ids: Vec<u64> = self.page_table.keys().cloned().collect();
        for pid in page_ids {
            self.flush_page(pid)?;
        }
        Ok(())
    }

    pub fn unpin_page(&mut self, page_id: u64, is_dirty: bool) -> Result<()> {
        if let Some(&frame_id) = self.page_table.get(&page_id) {
            let page = &mut self.pages[frame_id as usize];
            if page.pin_count == 0 {
                return Err(BufferPoolError::PinCountError);
            }
            page.pin_count -= 1;
            page.is_dirty |= is_dirty;
            if page.pin_count == 0 {
                self.replacer.unpin(frame_id);
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

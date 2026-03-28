use crate::disk::DiskManager;
use common::{MAX_POOL_SIZE, MAX_PAGE_SIZE}; // TODO: NEED TO SEE ABOUT THIS SIZE
use std::{collections::HashMap, fmt::Error};

#[derive(Clone, Copy)]
struct PeriodicFlusher;

impl PeriodicFlusher {
    pub fn victim(self) -> u64 {
        todo!()
    }

    pub fn unpin(self, frame_id: u64) {
        todo!()
    }

    pub fn pin(self, frame_id: u64) {
        todo!()
    }
}
// TODO: NEED TO IMPLEMENT A BACKGROUND TASK TO FLUSH THE EVICTED PAGES TO IMRPOVE THE PERFORMANCE

#[derive(Clone, Copy)]
struct Page {
    pub id: u64, 
    pub pin_count: u64,
    pub is_dirty: bool,
    pub data: [u64; MAX_PAGE_SIZE],
}

pub struct BufferPoolManager {
    pub disk_manager: DiskManager,
    pub pages: [Page; MAX_POOL_SIZE],
    pub periodic_flusher: PeriodicFlusher,
    pub free_list: Vec<u64>,
    pub page_table: HashMap<u64, u64>,
}

impl BufferPoolManager {
    pub fn new(disk_manager: DiskManager, periodic_flusher: PeriodicFlusher) -> Self {
        Self {
            disk_manager,
            pages: [Page; MAX_POOL_SIZE],
            periodic_flusher,
            free_list: Vec::new(),
            page_table: HashMap::new(),
        }
    }

    pub fn new_page() -> Page {
    todo!()
}

pub fn fetch_page(&mut self, page_id: u64) -> Page {
    if let Some(frame_id) = self.page_table.get(&page_id) {
        let mut page = self.pages[*frame_id as usize];
        page.pin_count += 1;
        self.periodic_flusher.pin(*frame_id);
        return page;
    } else {todo!()}


}

pub fn flush_page(page_id: u64) -> bool {
    todo!()
}

pub fn flush_all_pages() {
    todo!()
}

pub fn delete_page(page_id: u64) -> Error {
    todo!()
}

pub fn unpin_page(page_id: u64, is_dirty: bool) -> Error {
    todo!()
}

}

pub fn get_frame_id(manager: BufferPoolManager) -> (u64, bool) {
    let list = manager.free_list;
    if list.len() > 0 {
        let frame_id = list[0];
        let new_list = Vec::from(list[1:]);
        return (frame_id, true);
    }
    return (manager.periodic_flusher.victim(), false);
}
 
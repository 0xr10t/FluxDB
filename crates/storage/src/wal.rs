//! Functionality for the Write Ahead Log
//! Must be durable and atomic
//! Should be able to logs to reconstruct db state

use crate::disk::PageId;
use std::path::PathBuf;

type Lsn = u64;

#[derive(Debug)]
pub enum WalError {}

pub type Result<T> = std::result::Result<T, WalError>;

pub enum LogRecord {
    /// Update to a single page (physical logging).
    PageUpdate {
        page_id: PageId,
        before: Vec<u8>,
        after: Vec<u8>,
    },

    /// Optional: allocate/free pages, etc.
    PageAllocate { page_id: PageId },
}

pub struct Wal {
    db_path: PathBuf, // Db file path
    logs: Vec<LogRecord>,
}

impl Wal {
    // Append log to Wal
    // Add checksum to end of log (hash of log)
    pub fn append() -> Result<()> {
        todo!()
    }

    // Sync the Wal to disk
    // Ensure WAL is durable upto lsn
    //* Should be atomic */
    // Called before writing dirty page from buffer pool to disk
    // make
    pub fn flush(&self, _lsn: Lsn) {
        todo!()
    }
}
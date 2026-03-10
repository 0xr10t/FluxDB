//! Functionality for the Write Ahead Log
//! Must be durable and atomic
//! Should be able to logs to reconstruct db state


use std::path::PathBuf;

type Lsn = u64;

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
    logs: Vec<Log>,
}

impl Wal {
    // Append log to Wal
    // Add checksum to end of log (hash of log)
    pub fn append() -> Result<> {}

    // Sync the Wal to disk
    // Ensure WAL is durable upto lsn
    //* Should be atomic */
    // Called before writing dirty page from buffer pool to disk
    // make
    pub fn flush(lsn: Lsn) {}
}